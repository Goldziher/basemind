"""Contract tests for the basemind Hermes Agent plugin (basemind/hermes.py).

These use a duck-typed fake ``ctx`` — no Hermes dependency — and assert the plugin:

* registers every bundled skill and slash command,
* wires the two comms hooks,
* is fully fail-open (runs with a ``ctx`` missing methods, and with no ``basemind`` binary),
* ships its manifest + bundled assets where the wheel's package-data expects them.
"""

from __future__ import annotations

import importlib.util
import tomllib
from pathlib import Path

import pytest

import basemind
from basemind import hermes

PKG_DIR = Path(hermes.__file__).resolve().parent
PIP_PKG_ROOT = PKG_DIR.parent


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
    assert callable(basemind.register)
    assert basemind.register is hermes.register


def test_register_wires_all_skills():
    ctx = RecordingCtx()
    hermes.register(ctx)
    names = {name for name, _ in ctx.skills}
    assert "basemind" in names
    assert len(ctx.skills) >= 8
    for _, path in ctx.skills:
        assert Path(path).is_file()


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
    monkeypatch.setattr(hermes, "_run_basemind_json", lambda *a, **k: None)
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


def test_session_start_context_lists_messages(monkeypatch):
    monkeypatch.setattr(
        hermes,
        "_run_basemind_json",
        lambda *a, **k: {"messages": [{"subject": "hi", "from": "peer", "id": "m1", "ts_micros": 5}]},
    )
    text = hermes._session_start_context("/tmp")
    assert "[hi] from peer (id: m1)" in text
    assert hermes._DISCIPLINE in text


def test_delta_context_baselines_then_reports_new(monkeypatch, tmp_path):
    monkeypatch.setattr(hermes, "tempfile", hermes.tempfile)
    monkeypatch.setattr(hermes.tempfile, "gettempdir", lambda: str(tmp_path))
    msgs = [{"subject": "a", "from": "p", "id": "m1", "ts_micros": 10}]
    monkeypatch.setattr(hermes, "_run_basemind_json", lambda *a, **k: {"messages": msgs})
    assert hermes._delta_context("/tmp", "sid") is None
    msgs.append({"subject": "b", "from": "p", "id": "m2", "ts_micros": 20})
    out = hermes._delta_context("/tmp", "sid")
    assert out is not None and "id: m2" in out and "id: m1" not in out


def test_pyproject_declares_the_hermes_entry_point():
    data = tomllib.loads((PIP_PKG_ROOT / "pyproject.toml").read_text())
    entry = data["project"]["entry-points"]["hermes_agent.plugins"]
    assert entry == {"basemind": "basemind"}
    pkg_data = data["tool"]["setuptools"]["package-data"]["basemind"]
    assert "plugin.yaml" in pkg_data


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
    assert any(n.endswith("basemind/plugin.yaml") for n in names)
    assert any("basemind/skills/" in n and n.endswith("SKILL.md") for n in names)
    ep = next(n for n in names if n.endswith("entry_points.txt"))
    with zipfile.ZipFile(wheels[0]) as zf:
        assert "hermes_agent.plugins" in zf.read(ep).decode()
