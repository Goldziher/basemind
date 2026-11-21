# AI-RULEZ :: GENERATED FILE — DO NOT EDIT
# Content-Hash: blake3:a08ce6befad51c35d7d7d264edc5929474ce119ca3f07adc1f7b5c1c34650f47
# Source-Hash: blake3:a6c68a481d542a53e505749532735ddcb726e4d2c29eaac7fffe232cf5cf94fc
# Schema-Version: v1

"""Hermes Agent plugin registration for basemind.

The plugin registers public skills, slash commands and scoped agent-comms notifications.
A lazy SDK bridge exposes the installed local MCP server through ``basemind_call``;
existing manually configured Hermes MCP servers remain available independently.

Design constraints (all load-bearing):

* **stdlib-only** — the same package is imported by the ``basemind`` CLI, so this module must
  not pull in Hermes or any third-party dependency at import time.
* **import-cheap and side-effect-free** at module load.
* **required registration is explicit** — supported skill and MCP registration errors reach
  Hermes' plugin manager, which reports the failed plugin without breaking Hermes startup.
  Missing optional context methods and notification failures remain best effort. The binary
  is required only when a tool or notification invokes it, not while importing this module.

Hermes calls :func:`register` with a ``PluginContext`` once, at load. The comms hooks close over
that context so they can call ``ctx.inject_message(...)`` when they fire.
"""

from __future__ import annotations

import asyncio
import json
import os
import shutil
import subprocess
import warnings
from builtins import BaseExceptionGroup, ExceptionGroup
from datetime import timedelta
from pathlib import Path

_PKG_DIR = Path(__file__).resolve().parent
_SKILLS_DIR = _PKG_DIR / "skills"
_COMMANDS_DIR = _PKG_DIR / "commands"

_DISCIPLINE = (
    "basemind is available over MCP in this session: a tree-sitter code map, git history, "
    "document search and shared memory. Each job is its own tool with an exact schema; an unknown "
    "argument is rejected and line numbers are 1-based. "
    "Prefer it over grep/read for structural and historical questions: its tools return paths, "
    "line numbers, and signatures, not file bodies, so they cost a fraction of the tokens of "
    'reading source. Default workflow: code_outline {"path":"src/store.rs"} before opening a file, '
    'then code_read {"path":"src/store.rs","symbol":"open"} for only the body you need; '
    'code_symbols {"name":"open_store"} instead of grep for a definition; '
    'code_refs {"name":"open_store"} instead of grepping call sites; '
    'code_grep {"pattern":"cache_root("} instead of shelling out to ripgrep, also for exact text in '
    "markdown, config and data files; "
    'find_files {"query":"readme"} to locate a file; git_log {}, git_blame {"path":"src/store.rs"} '
    "and git_diff {} instead of git log, blame, status and diff; "
    'docs_search {"query":"how releases are versioned"} to search documents by meaning, and PDFs, '
    "Office files, HTML and email. The index follows your edits: if a result says stale or a new "
    'file is missing, index_rescan {"paths":["src/store.rs"]} for that file, not a reconnect; '
    "index_rescan {} only when index_status reports an empty index, degraded or rescan_needed, "
    "or after a change under scan.extra_roots (paths skips those). "
    "Do not re-read a file basemind already mapped."
)

_COMMS_TOOLS = (
    "Agent messaging is opt-in through three deferred tools, found by tool search: agent_post to "
    "send (subject starts a thread, thread or reply_to continues one), agent_inbox to read unread "
    "messages (bodies inline; reading marks them read, peek does not) and agent_threads to find, "
    "join, leave, invite to or archive threads. A subagent that wants its own inbox passes as."
)


def register(ctx) -> None:
    """Register basemind's skills, slash commands, and comms hooks with Hermes.

    Required capabilities fail with context; optional notifications remain best effort.
    """
    skill_paths = _required_skill_paths(ctx)
    _register_mcp_tools(ctx)
    _register_skills(ctx, skill_paths)
    for step in (_register_commands, _register_hooks):
        try:
            step(ctx)
        except Exception as error:
            warnings.warn(f"Optional basemind registration failed: {error}", RuntimeWarning, stacklevel=2)


def _required_skill_paths(ctx) -> list[Path]:
    """Validate required resource paths before mutating the host's registries."""
    if not callable(getattr(ctx, "register_skill", None)):
        return []
    if not _SKILLS_DIR.is_dir():
        raise RuntimeError("Required basemind skills directory is missing")
    directories = sorted(path for path in _SKILLS_DIR.iterdir() if path.is_dir())
    if not directories:
        raise RuntimeError("No required basemind skills were found")
    paths = []
    for directory in directories:
        skill_md = directory / "SKILL.md"
        try:
            if not skill_md.is_file():
                raise FileNotFoundError(f"SKILL.md is missing from {directory.name}")
            name = _front_matter_value(skill_md.read_text(encoding="utf-8"), "name")
            if name != directory.name:
                raise ValueError(f"Skill directory {directory.name} does not match its declared name {name}")
            if not name.isascii() or not all(character.isalnum() or character in "_-" for character in name):
                raise ValueError(
                    f"Invalid Hermes skill name {name!r}; use ASCII letters, digits, underscores or hyphens"
                )
        except Exception as error:
            raise RuntimeError(f"Failed to validate required basemind skill {directory.name}: {error}") from error
        paths.append(skill_md)
    return paths


def _register_skills(ctx, paths: list[Path]) -> None:
    reg = getattr(ctx, "register_skill", None)
    for skill_md in paths:
        try:
            reg(skill_md.parent.name, skill_md)
        except Exception as error:
            raise RuntimeError(f"Failed to register basemind skill {skill_md.parent.name}: {error}") from error


def _register_commands(ctx) -> None:
    reg = getattr(ctx, "register_command", None)
    if not callable(reg) or not _COMMANDS_DIR.is_dir():
        return
    for cmd_md in sorted(_COMMANDS_DIR.glob("*.md")):
        body = _read_text(cmd_md)
        if not body:
            continue
        name = cmd_md.stem
        description = _front_matter_value(body, "description") or f"basemind {name} command"
        _safe_call(reg, name, _make_command_handler(body), description)


def _make_command_handler(body: str):
    """A command handler returns the command's markdown (its agent instructions) verbatim.

    These ``.md`` files are the same prompt definitions the Claude/Codex plugins expand, so
    returning the body gives Hermes faithful slash-command parity without shelling anything.
    """

    def handler(_args: str = "") -> str:
        return body

    return handler


def _register_hooks(ctx) -> None:
    reg = getattr(ctx, "register_hook", None)
    inject = getattr(ctx, "inject_message", None)
    if not callable(reg) or not callable(inject):
        return

    def _inject(text):
        if not text:
            return
        if not _safe_call(inject, text):
            _safe_call(inject, text, "user")

    def on_session_start(**_kwargs):
        _inject(_session_start_context(os.getcwd()))

    def pre_llm_call(**_kwargs):
        _inject(_delta_context(os.getcwd()))

    _safe_call(reg, "on_session_start", on_session_start)
    _safe_call(reg, "pre_llm_call", pre_llm_call)


def _session_start_context(cwd: str) -> str:
    """Boot context: the operating discipline, the deferred agent tools, and a count of unread messages."""
    messages = _inbox_messages(cwd, ["--peek"], 5)
    if messages:
        return f"{_DISCIPLINE} {_COMMS_TOOLS} You have {len(messages)} unread agent message(s); read them with agent_inbox."
    return f"{_DISCIPLINE} {_COMMS_TOOLS}"


def _delta_context(cwd: str) -> str | None:
    """Per-turn delta: an announcing read lists unread messages once, as front matter, and marks none read.

    Hermes cannot rewrite a tool call's arguments, so its agent tool calls act as the principal and
    this read does too: a session-scoped name would announce to an actor that never reads.
    """
    messages = _inbox_messages(cwd, ["--announce"], 10)
    if not messages:
        return None
    return (
        "Unread agent messages from basemind (announced once, not yet read):\n"
        f"{_format_lines(messages)}\n"
        "Read them with agent_inbox (bodies inline; id reads one whole message). Reply with agent_post if warranted."
    )


def _inbox_messages(cwd: str, flags: list[str], limit: int):
    """Return the inbox message list (possibly empty), or ``None`` when the daemon is unavailable."""
    out = _run_basemind(["agent", "inbox", *flags, "--root", cwd, "--json", "--limit", str(limit)], timeout=6)
    if not out:
        return None
    try:
        data = json.loads(out)
    except Exception:
        return None
    messages = data.get("messages") if isinstance(data, dict) else None
    return messages if isinstance(messages, list) else []


def _run_basemind(args, timeout, stdin=None):
    try:
        proc = subprocess.run(  # noqa: S603,S607 - fixed argv, no shell
            ["basemind", *args],
            input=stdin,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
    except Exception:
        return None
    if proc.returncode != 0 or not proc.stdout.strip():
        return None
    return proc.stdout


def _format_lines(messages) -> str:
    return "\n".join(
        f"  - from {m.get('from', '')} in {m.get('subject', '')} ({m.get('id', '')}, {m.get('body_bytes', 0)} bytes)"
        for m in messages
    )


def _front_matter_value(body: str, key: str) -> str | None:
    """Read a generated single-line scalar from a leading frontmatter block."""
    if not body.startswith("---"):
        return None
    end = body.find("\n---", 3)
    if end == -1:
        return None
    for line in body[3:end].splitlines():
        stripped = line.strip()
        if stripped.startswith(f"{key}:"):
            return stripped[len(key) + 1 :].strip().strip("\"'") or None
    return None


def _safe_call(fn, *args) -> bool:
    """Report optional registration failures while preserving compatibility fallbacks."""
    try:
        fn(*args)
        return True
    except Exception as error:
        warnings.warn(f"Optional basemind registration failed: {error}", RuntimeWarning, stacklevel=2)
        return False


def _read_text(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8", errors="replace")
    except Exception:
        return ""


PLUGIN_NAME = "basemind"
CLI_COMMAND = "basemind"
SERVER_ARGUMENTS = ["serve"]
TOOL_NAMES = [
    "code_outline",
    "code_symbols",
    "code_read",
    "code_grep",
    "code_search",
    "code_refs",
    "find_files",
    "docs_search",
    "git_log",
    "git_blame",
    "git_diff",
    "memory_save",
    "memory_search",
    "index_status",
    "index_rescan",
    "agent_post",
    "agent_inbox",
    "agent_threads",
]
DISCOVERY_TOOL = "list_tools"
REQUEST_TIMEOUT_SECONDS = 300


def error_details(error: BaseException) -> str:
    """Expose SDK task-group causes instead of hiding the actionable failure."""
    if isinstance(error, BaseExceptionGroup):
        return "; ".join(error_details(cause) for cause in error.exceptions)[:1024]
    return str(error)[:1024]


async def call_tool(arguments: dict[str, object], **_kwargs: object) -> dict[str, object]:
    """Call one supported MCP tool using the installed local CLI."""
    from mcp.client.stdio import stdio_client
    from mcp.shared.exceptions import McpError

    from mcp import ClientSession, StdioServerParameters

    if not isinstance(arguments, dict) or set(arguments) - {"tool", "tool_arguments"}:
        return {"error": "Pass an object containing tool and optional tool_arguments."}
    tool = arguments.get("tool")
    parameters = arguments.get("tool_arguments", {})
    if tool not in [*TOOL_NAMES, DISCOVERY_TOOL] or not isinstance(parameters, dict):
        return {"error": "Choose a supported tool and provide an object of tool_arguments."}
    command = shutil.which(CLI_COMMAND)
    if command is None:
        return {"error": f"Install {CLI_COMMAND} with MCP support and put it on PATH; see the plugin README."}

    server = StdioServerParameters(command=command, args=SERVER_ARGUMENTS, env=os.environ.copy())
    try:
        async with asyncio.timeout(REQUEST_TIMEOUT_SECONDS):
            async with (
                stdio_client(server) as (read, write),
                ClientSession(read, write, read_timeout_seconds=timedelta(seconds=REQUEST_TIMEOUT_SECONDS)) as session,
            ):
                await session.initialize()
                if tool == DISCOVERY_TOOL:
                    result = await session.list_tools()
                else:
                    result = await session.call_tool(tool, parameters)
                return result.model_dump(mode="json", exclude_none=True)
    except TimeoutError:
        return {"error": f"{CLI_COMMAND} MCP request timed out; reduce the workload or inspect server diagnostics."}
    except (McpError, OSError, ExceptionGroup, ValueError) as error:
        return {"error": f"{CLI_COMMAND} MCP request failed: {error_details(error)}"}


async def handle_tool(arguments: dict[str, object], **kwargs: object) -> str:
    """Return the native text contract while preserving structured MCP fields as JSON."""
    return json.dumps(await call_tool(arguments, **kwargs), ensure_ascii=False)


def _register_mcp_tools(ctx) -> None:
    """Register every bundled skill and one tool spanning the native MCP surface."""
    reg = getattr(ctx, "register_tool", None)
    if not callable(reg):
        return
    name = PLUGIN_NAME.replace("-", "_") + "_call"
    description = f"Call a {PLUGIN_NAME} tool on the installed local MCP server. Select list_tools to inspect request schemas; use namespaced skills for workflows."
    schema = {
        "name": name,
        "description": description,
        "parameters": {
            "type": "object",
            "properties": {
                "tool": {"type": "string", "enum": [*TOOL_NAMES, DISCOVERY_TOOL]},
                "tool_arguments": {"type": "object", "description": "Arguments accepted by the selected MCP tool."},
            },
            "required": ["tool"],
            "additionalProperties": False,
        },
    }
    try:
        reg(name=name, toolset=PLUGIN_NAME, schema=schema, handler=handle_tool, is_async=True)
    except Exception as error:
        raise RuntimeError(f"Failed to register basemind MCP tool {name}: {error}") from error
