"""Run a packaged release archive the way a user's machine runs it.

Usage:
    python3 scripts/smoke_release_archive.py <archive> <expected-version>

The archive is extracted into a fresh directory; it must carry ``LICENSE``, ``ATTRIBUTIONS.md`` and
``THIRD_PARTY_LICENSES`` at its root, the notices of the native code linked into the binary
(``LINKED_NOTICES``) and ``licenses/<name>.txt`` for each library it bundles (under ``lib/``, or a
DLL beside the Windows binary), and the binary in it must:

1. report ``basemind <expected-version>`` from ``--version``;
2. ``scan`` a small git fixture repository;
3. answer ``initialize`` and ``tools/list`` over ``serve``'s stdio, advertising at least one tool,
   and answer one ``tools/call`` of an advertised read-only tool without ``isError``;
4. open the agent store, which ``basemind agent status`` reports as ``open``.

Every run also sets the test hooks a ``test-support`` build reads (``TEST_HOOKS``, which
``tests/layering.rs`` keeps complete), each to a value a binary that read it trips on: the fault
lane aborts the scan, and the store-open delay holds the agent store at ``opening`` past step 4's
deadline; the scan gate names an existing file, which holds every scan of a binary that read it.
The account home is read only by ``serve --lead``, which this smoke does not start.

Every run gets its own ``BASEMIND_DATA_HOME``, ``BASEMIND_COMMS_DIR``, ``HOME`` (with the Windows
``AppData`` folders a real profile has) and cache directory, so nothing reaches a real session's index
or daemon; daemons it started are stopped at the end.
``LD_LIBRARY_PATH`` is removed so a Linux binary loads the libraries bundled beside it, as it does
on a machine that never built it.
"""

from __future__ import annotations

import json
import os
import queue
import shutil
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import zipfile
from pathlib import Path

TIMEOUT = 180
NOTICES = ("LICENSE", "ATTRIBUTIONS.md", "THIRD_PARTY_LICENSES")
# The same list as LINKED_NOTICES in scripts/package-release.sh.
LINKED_NOTICES = (
    "licenses/onnxruntime.txt",
    "licenses/onnxruntime-ThirdPartyNotices.txt",
    "licenses/tesseract.txt",
    "licenses/leptonica.txt",
)


def missing_notices(root: Path) -> list[str]:
    """The notice files, relative to ``root``, that an extracted archive lacks."""
    bundled = [*(root / "lib").glob("*"), *root.glob("*.dll")]
    expected = [*NOTICES, *LINKED_NOTICES, *(f"licenses/{library.name}.txt" for library in sorted(bundled))]
    return [notice for notice in expected if not (root / notice).is_file()]


# Seconds `basemind agent status` may report the agent store as `opening` before the smoke fails.
STORE_OPEN_DEADLINE = 60

STORE_POLL_INTERVAL = 0.5

# The test hooks a `test-support` build reads, each set to a value that build trips on.
TEST_HOOKS = {
    "BASEMIND_TEST_FAULT_LANE": "resolve:abort",
    "BASEMIND_TEST_COMMS_STORE_OPEN_DELAY_MS": str(60 * 60 * 1000),
}


def extract(archive: Path, into: Path) -> Path:
    if archive.name.endswith(".zip"):
        with zipfile.ZipFile(archive) as bundle:
            bundle.extractall(into)
        binary = into / "basemind.exe"
    else:
        with tarfile.open(archive, "r:gz") as bundle:
            bundle.extractall(into, filter="tar")
        binary = into / "basemind"
    if not binary.is_file():
        raise SystemExit(f"{archive.name}: no {binary.name} at the archive root")
    missing = missing_notices(into)
    if missing:
        raise SystemExit(f"{archive.name}: missing notices: {', '.join(missing)}")
    binary.chmod(0o755)
    return binary


def sandbox_env(root: Path) -> dict[str, str]:
    env = {k: v for k, v in os.environ.items() if k != "LD_LIBRARY_PATH" and not k.startswith("BASEMIND_")}
    for name in ("data", "comms", "home", "cache"):
        (root / name).mkdir(parents=True, exist_ok=True)
    # Windows resolves LocalAppData (the grammar cache's home) as %USERPROFILE%\AppData\Local and
    # reports no folder when that directory is missing, so the sandbox profile carries both.
    local, roaming = root / "home" / "AppData" / "Local", root / "home" / "AppData" / "Roaming"
    local.mkdir(parents=True, exist_ok=True)
    roaming.mkdir(parents=True, exist_ok=True)
    env.update(
        BASEMIND_DATA_HOME=str(root / "data"),
        BASEMIND_COMMS_DIR=str(root / "comms"),
        HOME=str(root / "home"),
        USERPROFILE=str(root / "home"),
        LOCALAPPDATA=str(local),
        APPDATA=str(roaming),
        XDG_CACHE_HOME=str(root / "cache"),
        NO_COLOR="1",
    )
    env.update(TEST_HOOKS)
    env["BASEMIND_TEST_ACCOUNT_HOME"] = str(root / "test-account-home")
    scan_gate = root / "hold-scan-gate"
    scan_gate.write_text("", encoding="utf-8")
    env["BASEMIND_TEST_HOLD_SCAN"] = str(scan_gate)
    return env


def fixture(root: Path) -> Path:
    repo = root / "repo"
    (repo / "src").mkdir(parents=True)
    (repo / "src" / "lib.rs").write_text("pub fn alpha(x: u32) -> u32 {\n    x + 1\n}\n", encoding="utf-8")
    (repo / "basemind.toml").write_text('"$schema" = "v1"\n\n[documents]\nembed = false\n', encoding="utf-8")
    git = ["git", "-c", "user.name=smoke", "-c", "user.email=smoke@example.invalid", "-c", "commit.gpgsign=false"]
    subprocess.run([*git, "init", "-q"], cwd=repo, check=True)
    subprocess.run([*git, "add", "."], cwd=repo, check=True)
    subprocess.run([*git, "commit", "-qm", "fixture"], cwd=repo, check=True)
    return repo


def run(binary: Path, args: list[str], env: dict[str, str], timeout: float = TIMEOUT) -> str:
    try:
        done = subprocess.run([str(binary), *args], env=env, capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        raise SystemExit(f"basemind {' '.join(args)} did not finish within {timeout:.0f}s") from None
    if done.returncode != 0:
        raise SystemExit(f"basemind {' '.join(args)} exited {done.returncode}:\n{done.stderr.strip()}")
    return done.stdout


class Server:
    def __init__(self, binary: Path, repo: Path, env: dict[str, str]) -> None:
        self.proc = subprocess.Popen(
            [str(binary), "--root", str(repo), "serve"],
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        self.lines: queue.Queue[str | None] = queue.Queue()
        threading.Thread(target=self._pump, daemon=True).start()
        self.next_id = 0

    def _pump(self) -> None:
        assert self.proc.stdout is not None
        for line in self.proc.stdout:
            self.lines.put(line)
        self.lines.put(None)

    def send(self, message: dict) -> None:
        assert self.proc.stdin is not None
        self.proc.stdin.write(json.dumps(message) + "\n")
        self.proc.stdin.flush()

    def request(self, method: str, params: dict) -> dict:
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params})
        while True:
            try:
                line = self.lines.get(timeout=TIMEOUT)
            except queue.Empty:
                raise SystemExit(f"serve did not answer {method} within {TIMEOUT}s") from None
            if line is None:
                raise SystemExit(f"serve closed stdout before answering {method} (exit {self.proc.wait()})")
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                continue
            if message.get("id") == self.next_id and "method" not in message:
                if "error" in message:
                    raise SystemExit(f"serve refused {method}: {message['error']}")
                return message["result"]

    def close(self) -> None:
        self.proc.kill()
        self.proc.wait()


def agent_store_state(binary: Path, repo: Path, env: dict[str, str]) -> str:
    """The agent store's state once it is no longer ``opening``, within ``STORE_OPEN_DEADLINE``.

    Each ``agent status`` gets only the time left before the deadline, and an answer that arrives
    after it fails the smoke whatever state it reports.
    """
    deadline = time.monotonic() + STORE_OPEN_DEADLINE
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise SystemExit(f"the agent store was still opening after {STORE_OPEN_DEADLINE}s")
        status = json.loads(run(binary, ["--root", str(repo), "--json", "agent", "status"], env, remaining))
        if time.monotonic() > deadline:
            raise SystemExit(f"agent status answered after the {STORE_OPEN_DEADLINE}s deadline")
        state = status["store"]["state"]
        if state != "opening":
            return state
        time.sleep(STORE_POLL_INTERVAL)


def probe_call(tools: list[dict]) -> tuple[str, dict]:
    """A read-only call the fixture can answer, for the tool surface this binary advertises."""
    names = {tool["name"] for tool in tools}
    if "code_outline" in names:
        return "code_outline", {"path": "src/lib.rs"}
    if "code" in names:
        return "code", {"mode": "outline", "path": "src/lib.rs"}
    raise SystemExit(f"neither code_outline nor code is advertised: {sorted(names)}")


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    archive, expected = Path(argv[0]).resolve(), argv[1]
    work = Path(tempfile.mkdtemp(prefix="basemind-archive-smoke-"))
    binary = extract(archive, work / "unpacked")
    env = sandbox_env(work)
    try:
        reported = run(binary, ["--version"], env).strip()
        if reported != f"basemind {expected}":
            raise SystemExit(f"{archive.name}: --version reports {reported!r}, expected 'basemind {expected}'")
        print(f"{archive.name}: --version -> {reported}")
        repo = fixture(work)
        run(binary, ["--root", str(repo), "scan"], env)
        print(f"{archive.name}: scan of the fixture succeeded")
        server = Server(binary, repo, env)
        try:
            init = server.request(
                "initialize",
                {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "release-archive-smoke", "version": "0"},
                },
            )
            server.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
            tools = server.request("tools/list", {}).get("tools", [])
            if not tools:
                raise SystemExit(f"{archive.name}: serve advertised no tools")
            name, arguments = probe_call(tools)
            result = server.request("tools/call", {"name": name, "arguments": arguments})
            text = "".join(block.get("text", "") for block in result.get("content", []))
            if result.get("isError") or "alpha" not in text:
                raise SystemExit(f"{archive.name}: {name} did not outline the fixture: {text[:400]}")
        finally:
            server.close()
        print(
            f"{archive.name}: serve ({init.get('serverInfo', {}).get('name')}) advertised {len(tools)} tools; "
            f"{name} outlined the fixture"
        )
        state = agent_store_state(binary, repo, env)
        if state != "open":
            raise SystemExit(f"{archive.name}: agent status reports the store {state!r}, expected 'open'")
        print(f"{archive.name}: agent status reports the store open")
    finally:
        subprocess.run(
            [str(binary), "--root", str(work / "repo"), "comms", "stop"],
            env=env,
            capture_output=True,
            timeout=TIMEOUT,
            check=False,
        )
        shutil.rmtree(work, ignore_errors=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
