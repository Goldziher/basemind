"""Contract tests for the Hermes MCP bridge (``call_tool``) against a real stdio MCP server.

A stub ``basemind`` executable on ``PATH`` serves one tool through the pinned MCP SDK, so these tests
exercise the same client session, timeout, error and serialisation code the published plugin runs.
"""

from __future__ import annotations

import asyncio
import os
import sys
from pathlib import Path

import pytest

try:
    from basemind_hermes_plugin import hermes
except ImportError:
    import basemind_hermes_plugin as hermes

STUB_SERVER = """\
import sys

from mcp.server.mcpserver import MCPServer

if sys.argv[1:] != ["serve"]:
    sys.exit(f"unexpected arguments: {sys.argv[1:]}")

server = MCPServer("basemind")


@server.tool(description="Report the index state.")
def index_status(path: str) -> dict[str, object]:
    return {"path": path, "files": 3}


server.run()
"""

CRASHING_SERVER = """\
import sys

sys.exit(3)
"""


def install_cli(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, source: str) -> None:
    """Put an executable named like the CLI first on PATH, running ``source`` with this interpreter."""
    script = tmp_path / "server.py"
    script.write_text(source)
    cli = tmp_path / hermes.CLI_COMMAND
    cli.write_text(f'#!/bin/sh\nexec "{sys.executable}" "{script}" "$@"\n')
    cli.chmod(0o755)
    monkeypatch.setenv("PATH", f"{tmp_path}{os.pathsep}{os.environ['PATH']}")


@pytest.mark.skipif(sys.platform == "win32", reason="the stub CLI is a POSIX shell script")
def test_call_tool_returns_the_wire_shaped_result(tmp_path, monkeypatch):
    install_cli(tmp_path, monkeypatch, STUB_SERVER)
    result = asyncio.run(hermes.call_tool({"tool": "index_status", "tool_arguments": {"path": "src"}}))
    assert result == {
        "content": [{"type": "text", "text": '{\n  "path": "src",\n  "files": 3\n}'}],
        "structuredContent": {"path": "src", "files": 3},
        "isError": False,
        "resultType": "complete",
    }


@pytest.mark.skipif(sys.platform == "win32", reason="the stub CLI is a POSIX shell script")
def test_call_tool_lists_tools_with_wire_field_names(tmp_path, monkeypatch):
    install_cli(tmp_path, monkeypatch, STUB_SERVER)
    result = asyncio.run(hermes.call_tool({"tool": hermes.DISCOVERY_TOOL}))
    assert [tool["name"] for tool in result["tools"]] == ["index_status"]
    assert result["tools"][0]["inputSchema"]["required"] == ["path"]


@pytest.mark.skipif(sys.platform == "win32", reason="the stub CLI is a POSIX shell script")
def test_call_tool_reports_a_server_that_exits_before_answering(tmp_path, monkeypatch):
    install_cli(tmp_path, monkeypatch, CRASHING_SERVER)
    result = asyncio.run(hermes.call_tool({"tool": "index_status", "tool_arguments": {"path": "src"}}))
    assert list(result) == ["error"]
    assert result["error"].startswith(f"{hermes.CLI_COMMAND} MCP request failed: ")
