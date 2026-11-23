"""Contract tests for the basemind Hermes Agent plugin (basemind_hermes_plugin/hermes.py).

These use a duck-typed fake ``ctx`` — no Hermes dependency — and assert the plugin:

* registers every bundled skill and slash command,
* wires the two comms hooks,
* propagates required registration errors and supports missing optional context methods,
* ships its manifest + bundled assets where the wheel's package-data expects them.
"""

from __future__ import annotations

import asyncio
import contextlib
import importlib.util
import json
import os
import shutil
import socket
import sys
import tempfile
import tomllib
from pathlib import Path

import basemind_hermes_plugin
import pytest

try:
    from basemind_hermes_plugin import hermes
except ImportError:
    hermes = basemind_hermes_plugin

PKG_DIR = Path(hermes.__file__).resolve().parent
PIP_PKG_ROOT = PKG_DIR.parents[1] if PKG_DIR.parent.name == "src" else PKG_DIR.parent


class RecordingCtx:
    """A PluginContext stand-in that records every registration call."""

    def __init__(self):
        self.skills = []
        self.commands = []
        self.hooks = {}
        self.injected = []

    def register_skill(self, name, path):
        self.skills.append((name, path))

    def register_command(self, name, handler, description=""):
        self.commands.append((name, handler, description))

    def register_hook(self, event, callback):
        self.hooks[event] = callback

    def inject_message(self, content, role="user"):
        self.injected.append((role, content))


def test_register_is_exported_from_package():
    assert callable(basemind_hermes_plugin.register)
    assert basemind_hermes_plugin.register is hermes.register


def test_register_wires_all_skills():
    ctx = RecordingCtx()
    hermes.register(ctx)
    names = {name for name, _ in ctx.skills}
    assert "basemind" in names
    assert len(ctx.skills) == 10
    for _, path in ctx.skills:
        assert isinstance(path, Path), "Native Hermes calls path.exists() on registered skills"
        assert path.is_file()


def test_should_propagate_required_skill_registration_error():
    class RejectingSkillCtx(RecordingCtx):
        def register_skill(self, name, path):
            raise ValueError(f"Invalid skill name: {name}")

    with pytest.raises(RuntimeError, match=r"basemind.*skill.*Invalid skill name"):
        hermes.register(RejectingSkillCtx())


@pytest.mark.parametrize("name", ["invalid:skill", "invalid skill", "invalíd"])
def test_should_reject_invalid_identity_before_registering_capabilities(tmp_path, monkeypatch, name):
    class RegisteringToolCtx(RecordingCtx):
        def __init__(self):
            super().__init__()
            self.tools = []

        def register_tool(self, **kwargs):
            self.tools.append(kwargs)

    skills = tmp_path / "skills"
    invalid = skills / name
    invalid.mkdir(parents=True)
    (invalid / "SKILL.md").write_text(f"---\nname: {name}\n---\nSkill body")
    monkeypatch.setattr(hermes, "_SKILLS_DIR", skills)
    ctx = RegisteringToolCtx()
    with pytest.raises(RuntimeError, match="Invalid Hermes skill name"):
        hermes.register(ctx)
    assert ctx.skills == []
    assert ctx.tools == []


def test_should_propagate_required_mcp_registration_error():
    class RejectingToolCtx(RecordingCtx):
        def register_tool(self, **kwargs):
            raise ValueError("Cannot register basemind_call")

    ctx = RejectingToolCtx()
    with pytest.raises(RuntimeError, match=r"basemind.*MCP.*basemind_call"):
        hermes.register(ctx)
    assert ctx.skills == []


def test_register_wires_expected_commands_with_body_handlers():
    ctx = RecordingCtx()
    hermes.register(ctx)
    by_name = {name: (handler, desc) for name, handler, desc in ctx.commands}
    assert {"bm", "bm-doctor", "bm-scan", "bm-stats"} <= set(by_name)
    handler, description = by_name["bm-doctor"]
    assert description
    body = handler("")
    assert isinstance(body, str) and len(body) > 0


def test_register_wires_comms_hooks():
    ctx = RecordingCtx()
    hermes.register(ctx)
    assert set(ctx.hooks) == {"on_session_start", "pre_llm_call"}


def test_hooks_are_fail_open_without_a_basemind_binary(monkeypatch):
    monkeypatch.setattr(hermes, "_run_basemind", lambda *a, **k: None)
    ctx = RecordingCtx()
    hermes.register(ctx)
    ctx.hooks["on_session_start"]()
    assert ctx.injected and hermes._DISCIPLINE in ctx.injected[-1][1]
    ctx.injected.clear()
    ctx.hooks["pre_llm_call"](task_id="s1")
    assert ctx.injected == []


def test_register_never_raises_on_a_minimal_ctx():
    class Bare:
        """A ctx with none of the register_* methods — plugin must no-op cleanly."""

    hermes.register(Bare())


def fake_cli(monkeypatch, inbox, name="session-1a2b3c4d"):
    """Stand in for the binary: `hook name` prints `name`, `inbox` prints `inbox` as JSON; calls are recorded."""
    calls = []

    def run(args, timeout, stdin=None):
        calls.append((list(args), stdin))
        if args[:3] == ["agent", "hook", "name"]:
            return name
        if args[:2] == ["agent", "inbox"]:
            return json.dumps(inbox)
        return None

    monkeypatch.setattr(hermes, "_run_basemind", run)
    monkeypatch.setattr(hermes, "_daemon_is_up", lambda: True)
    return calls


def test_session_start_context_names_the_agent_tools_and_counts_unread(monkeypatch):
    calls = fake_cli(monkeypatch, {"messages": [{"subject": "hi", "from": "peer", "id": "t-abcdef.1"}]})
    text = hermes._session_start_context("/tmp")
    assert "agent_post" in text and "agent_inbox" in text and "agent_threads" in text
    assert "You have 1 unread agent message(s)" in text
    assert hermes._DISCIPLINE in text
    args = calls[0][0]
    assert args[:2] == ["agent", "inbox"] and "--peek" in args and "--json" in args


def test_discipline_steers_a_rescan_to_the_edited_paths():
    assert (
        'if a result says stale or a new file is missing, index_rescan {"paths":["src/store.rs"]} for that '
        "file, not a reconnect; index_rescan {} only when index_status reports an empty "
        "index, degraded or rescan_needed, or after a change under scan.extra_roots (paths skips those)."
    ) in hermes._DISCIPLINE
    assert "index_rescan {} after" not in hermes._DISCIPLINE


def test_session_start_context_without_messages_has_no_count(monkeypatch):
    fake_cli(monkeypatch, {"messages": []})
    text = hermes._session_start_context("/tmp")
    assert "agent_threads" in text and "unread agent message" not in text


def test_delta_context_announces_as_the_principal_and_keeps_no_state(monkeypatch):
    # Hermes cannot rewrite a tool call's arguments, so its agent tools act as the principal.
    msgs = [{"subject": "auth", "from": "peer", "id": "t-abcdef.3", "body_bytes": 42}]
    calls = fake_cli(monkeypatch, {"messages": msgs})
    out = hermes._delta_context("/tmp")
    assert "from peer in auth (t-abcdef.3, 42 bytes)" in out
    assert "agent_inbox" in out
    args = calls[0][0]
    assert args[:2] == ["agent", "inbox"] and "--announce" in args and "--as" not in args
    assert all(call[0][:3] != ["agent", "hook", "name"] for call in calls)
    assert hermes._delta_context("/tmp") == out


def test_delta_context_is_silent_without_messages(monkeypatch):
    fake_cli(monkeypatch, {"messages": []})
    assert hermes._delta_context("/tmp") is None


def test_pyproject_declares_the_hermes_entry_point():
    data = tomllib.loads((PIP_PKG_ROOT / "pyproject.toml").read_text())
    entry = data["project"]["entry-points"]["hermes_agent.plugins"]
    assert entry == {"basemind": "basemind_hermes_plugin"}
    if "setuptools" in data["tool"]:
        pkg_data = data["tool"]["setuptools"]["package-data"]["basemind_hermes_plugin"]
        assert "plugin.yaml" in pkg_data
    else:
        packages = data["tool"]["hatch"]["build"]["targets"]["wheel"]["packages"]
        assert packages == ["src/basemind_hermes_plugin"]


def test_bundled_assets_match_package_data_globs():
    assert (PKG_DIR / "plugin.yaml").is_file()
    assert list(PKG_DIR.glob("skills/*/SKILL.md"))
    assert list(PKG_DIR.glob("commands/*.md"))


@pytest.mark.skipif(
    importlib.util.find_spec("build") is None,
    reason="`build` not installed",
)
def test_built_wheel_contains_manifest_and_skills(tmp_path):
    import subprocess
    import zipfile

    subprocess.run(
        ["python3", "-m", "build", "--wheel", "--outdir", str(tmp_path)],
        cwd=str(PIP_PKG_ROOT),
        check=True,
        capture_output=True,
    )
    wheels = list(tmp_path.glob("*.whl"))
    assert wheels, "no wheel produced"
    with zipfile.ZipFile(wheels[0]) as zf:
        names = zf.namelist()
    assert any(n.endswith("basemind_hermes_plugin/plugin.yaml") for n in names)
    assert any("basemind_hermes_plugin/skills/" in n and n.endswith("SKILL.md") for n in names)
    ep = next(n for n in names if n.endswith("entry_points.txt"))
    with zipfile.ZipFile(wheels[0]) as zf:
        assert "hermes_agent.plugins" in zf.read(ep).decode()


@pytest.fixture(autouse=True)
def fresh_process_state():
    """The version check and the MCP sessions are per-process state: start each test without them."""
    hermes._binary_notice.cache_clear()
    hermes._sessions.clear()
    yield
    hermes._binary_notice.cache_clear()
    hermes._sessions.clear()


@pytest.fixture
def comms_dir(monkeypatch):
    """A short comms directory (a Unix socket path is limited to ~100 bytes) named by the override."""
    with tempfile.TemporaryDirectory(dir="/tmp") as directory:
        monkeypatch.setenv("BASEMIND_COMMS_DIR", directory)
        yield Path(directory)


needs_unix_sockets = pytest.mark.skipif(sys.platform == "win32", reason="the daemon listens on a named pipe")


@needs_unix_sockets
def test_daemon_is_not_up_without_a_comms_directory(comms_dir):
    assert hermes._daemon_is_up() is False


@needs_unix_sockets
def test_daemon_is_not_up_for_a_socket_nobody_listens_on(comms_dir):
    (comms_dir / "g1").mkdir()
    orphan = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    orphan.bind(str(comms_dir / "g1" / "comms.sock"))
    orphan.close()
    assert (comms_dir / "g1" / "comms.sock").exists()
    assert hermes._daemon_is_up() is False


@needs_unix_sockets
def test_daemon_is_up_when_a_socket_accepts_connections(comms_dir):
    (comms_dir / "g7").mkdir()
    with contextlib.closing(socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)) as listener:
        listener.bind(str(comms_dir / "g7" / "comms.sock"))
        listener.listen()
        assert hermes._daemon_is_up() is True


def test_delta_context_spawns_nothing_while_no_daemon_runs(monkeypatch):
    calls = fake_cli(monkeypatch, {"messages": [{"subject": "hi", "from": "peer", "id": "t-abcdef.1"}]})
    monkeypatch.setattr(hermes, "_daemon_is_up", lambda: False)
    assert hermes._delta_context("/tmp") is None
    assert calls == []


def test_per_turn_inbox_read_has_a_short_timeout_and_session_start_keeps_its_own(monkeypatch):
    timeouts = {}

    def run(args, timeout, stdin=None):
        timeouts["--announce" if "--announce" in args else "--peek"] = timeout
        return json.dumps({"messages": []})

    monkeypatch.setattr(hermes, "_run_basemind", run)
    monkeypatch.setattr(hermes, "_daemon_is_up", lambda: True)
    hermes._delta_context("/tmp")
    hermes._session_start_context("/tmp")
    assert timeouts == {"--announce": 2, "--peek": 6}


def test_plugin_version_is_the_packages_version():
    assert hermes._plugin_version() == basemind_hermes_plugin.__version__


@pytest.mark.parametrize(
    ("output", "expected"),
    [
        ("basemind 0.34.0\n", None),
        ("basemind 0.34.9\n", None),
        ("basemind 0.35.0\n", "the binary on PATH is 0.35.0 but this plugin is 0.34.0"),
        ("basemind 1.34.0\n", "the binary on PATH is 1.34.0 but this plugin is 0.34.0"),
        ("something else\n", "`basemind --version` gave no version"),
        (None, "`basemind --version` gave no version"),
    ],
)
def test_binary_notice_compares_minor_versions(monkeypatch, output, expected):
    monkeypatch.setattr(hermes, "_plugin_version", lambda: "0.34.0")
    monkeypatch.setattr(hermes, "_run_basemind", lambda args, timeout, stdin=None: output)
    notice = hermes._binary_notice()
    if expected is None:
        assert notice is None
    else:
        assert expected in notice
        assert "pnpm add -g basemind@0.34.0" in notice and "pip install basemind==0.34.0" in notice


def test_binary_notice_runs_the_version_command_once(monkeypatch):
    calls = []

    def run(args, timeout, stdin=None):
        calls.append(list(args))
        return "basemind 0.35.0\n"

    monkeypatch.setattr(hermes, "_plugin_version", lambda: "0.34.0")
    monkeypatch.setattr(hermes, "_run_basemind", run)
    assert hermes._binary_notice() == hermes._binary_notice()
    assert calls == [["--version"]]


def test_session_start_injects_the_version_notice_only_on_a_mismatch(monkeypatch):
    fake_cli(monkeypatch, {"messages": []})
    monkeypatch.setattr(hermes, "_binary_notice", lambda: "basemind: skew")
    ctx = RecordingCtx()
    hermes.register(ctx)
    ctx.hooks["on_session_start"]()
    assert [text for _, text in ctx.injected][0] == "basemind: skew"
    assert hermes._DISCIPLINE in ctx.injected[1][1]

    ctx.injected.clear()
    monkeypatch.setattr(hermes, "_binary_notice", lambda: None)
    ctx.hooks["on_session_start"]()
    assert len(ctx.injected) == 1 and hermes._DISCIPLINE in ctx.injected[0][1]


class FakeResult:
    def __init__(self, payload):
        self.payload = payload

    def model_dump(self, mode="json", exclude_none=False):
        return self.payload


@pytest.fixture
def mcp_servers(monkeypatch):
    """Replace the SDK's stdio client with fakes; the returned list holds one entry per server spawned."""
    import mcp
    import mcp.client.stdio

    spawned = []

    @contextlib.asynccontextmanager
    async def stdio_client(server):
        spawned.append(server)
        yield None, None

    class ClientSession:
        def __init__(self, read, write, read_timeout_seconds=None):
            pass

        async def __aenter__(self):
            return self

        async def __aexit__(self, *exc_info):
            return False

        async def initialize(self):
            return None

        async def call_tool(self, tool, parameters):
            if tool == "index_status":
                raise OSError("the server went away")
            return FakeResult({"tool": tool, "arguments": parameters, "cwd": os.getcwd()})

        async def list_tools(self):
            return FakeResult({"tools": ["code_outline"]})

    monkeypatch.setattr(mcp.client.stdio, "stdio_client", stdio_client)
    monkeypatch.setattr(mcp, "ClientSession", ClientSession)
    monkeypatch.setattr(shutil, "which", lambda name: f"/opt/{name}")
    return spawned


def call(tool, **tool_arguments):
    return hermes.call_tool({"tool": tool, "tool_arguments": tool_arguments})


def test_calls_in_one_directory_share_one_server(mcp_servers):
    async def scenario():
        first = await call("code_outline", path="a.rs")
        second = await call("code_symbols", name="open")
        discovered = await call("list_tools")
        concurrent = await asyncio.gather(*(call("find_files", query=str(n)) for n in range(3)))
        return first, second, discovered, concurrent

    first, second, discovered, concurrent = asyncio.run(scenario())
    assert first["tool"] == "code_outline" and first["arguments"] == {"path": "a.rs"}
    assert second["tool"] == "code_symbols"
    assert discovered == {"tools": ["code_outline"]}
    assert [item["arguments"] for item in concurrent] == [{"query": "0"}, {"query": "1"}, {"query": "2"}]
    assert len(mcp_servers) == 1
    assert mcp_servers[0].command == "/opt/basemind" and mcp_servers[0].cwd == os.getcwd()


def test_each_working_directory_gets_its_own_server(mcp_servers, tmp_path, monkeypatch):
    async def scenario():
        await call("code_outline")
        monkeypatch.chdir(tmp_path)
        await call("code_outline")
        await call("code_outline")

    start = os.getcwd()
    asyncio.run(scenario())
    assert len(mcp_servers) == 2
    assert Path(mcp_servers[0].cwd) == Path(start)
    assert Path(mcp_servers[1].cwd).resolve() == tmp_path.resolve()


def test_a_failed_server_is_replaced_on_the_next_call(mcp_servers):
    async def scenario():
        await call("code_outline")
        failed = await call("index_status")
        recovered = await call("code_outline")
        return failed, recovered

    failed, recovered = asyncio.run(scenario())
    assert failed == {"error": "basemind MCP request failed: the server went away"}
    assert recovered["tool"] == "code_outline"
    assert len(mcp_servers) == 2


def test_a_new_event_loop_does_not_reuse_a_session_of_a_closed_one(mcp_servers):
    first = asyncio.run(call("code_outline"))
    second = asyncio.run(call("code_outline"))
    assert first["tool"] == second["tool"] == "code_outline"
    assert len(mcp_servers) == 2
