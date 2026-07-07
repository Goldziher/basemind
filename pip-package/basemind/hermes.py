"""Hermes Agent plugin registration for basemind.

basemind's tools reach Hermes through an MCP server declared in ``~/.hermes/config.yaml``
(``mcp_servers.basemind``) — a Hermes plugin cannot declare an MCP server. This module adds
what MCP config cannot: the basemind helper *skills*, *slash commands*, and agent-comms
*notifications* (parity with the Gemini/OpenCode plugin surfaces).

Design constraints (all load-bearing):

* **stdlib-only** — the same package is imported by the ``basemind`` CLI, so this module must
  not pull in Hermes or any third-party dependency at import time.
* **import-cheap and side-effect-free** at module load.
* **fail-open** — every registration and every hook is guarded; a missing ``basemind`` binary,
  a down comms broker, or a Hermes build that lacks a given ``ctx`` method must degrade to a
  no-op, never raise. A raising plugin would break both Hermes startup and the CLI.

Hermes calls :func:`register` with a ``PluginContext`` once, at load. The comms hooks close over
that context so they can call ``ctx.inject_message(...)`` when they fire.
"""

from __future__ import annotations

import json
import os
import subprocess
import tempfile
from pathlib import Path

_PKG_DIR = Path(__file__).resolve().parent
_SKILLS_DIR = _PKG_DIR / "skills"
_COMMANDS_DIR = _PKG_DIR / "commands"

# Context-economy operating discipline — mirrors hooks/session-start so the guidance an agent
# sees is identical across harnesses.
_DISCIPLINE = (
    "basemind is available over MCP in this session — a tree-sitter code map + git context. "
    "Prefer it over grep/read for structural and historical questions: its tools return paths, "
    "line numbers, and signatures, not file bodies, so they cost a fraction of the tokens of "
    "reading source. Default workflow: outline a file before opening it (then read only the span "
    "you need); search_symbols instead of grep for a definition; find_references/find_callers "
    "instead of grepping call sites; workspace_grep instead of shelling out to ripgrep; rescan "
    "after edits instead of reconnecting. Do not re-read a file basemind already mapped."
)

_COMMS_TOOLS = (
    "basemind first, shell/grep/git fallback — prefer basemind over grep, over naked git, and for "
    "docs/RAG/NER, web crawl, and parsing. You are connected to basemind agent-comms — a shared "
    "multi-agent chat. You have auto-joined every room scoped to this workspace. Levers: room_post "
    "{room, subject, body, reply_to?} to send (always give a short subject; the body holds the "
    "detail); room_history {room} and inbox_read to scan messages (these return front-matter only — "
    "subject/from/id — never bodies, to stay token-frugal); message_get {message_id} to read one "
    "body on demand; room_list to see rooms, room_join to join another. Prefer posting a concise "
    "status/question over staying silent when collaborating."
)


def register(ctx) -> None:
    """Register basemind's skills, slash commands, and comms hooks with Hermes.

    Each capability group is independently guarded so a failure in one (or a ``ctx`` that
    lacks a given ``register_*`` method) never prevents the others from registering.
    """
    for step in (_register_skills, _register_commands, _register_hooks):
        try:
            step(ctx)
        except Exception:  # pragma: no cover - defensive: never break plugin load
            pass


# ── Skills ──────────────────────────────────────────────────────────────────────────────────


def _register_skills(ctx) -> None:
    reg = getattr(ctx, "register_skill", None)
    if not callable(reg) or not _SKILLS_DIR.is_dir():
        return
    for skill_md in sorted(_SKILLS_DIR.glob("*/SKILL.md")):
        _safe_call(reg, skill_md.parent.name, str(skill_md))


# ── Slash commands ──────────────────────────────────────────────────────────────────────────


def _register_commands(ctx) -> None:
    reg = getattr(ctx, "register_command", None)
    if not callable(reg) or not _COMMANDS_DIR.is_dir():
        return
    for cmd_md in sorted(_COMMANDS_DIR.glob("*.md")):
        body = _read_text(cmd_md)
        if not body:
            continue
        name = cmd_md.stem
        description = _front_matter_description(body) or f"basemind {name} command"
        _safe_call(reg, name, _make_command_handler(body), description)


def _make_command_handler(body: str):
    """A command handler returns the command's markdown (its agent instructions) verbatim.

    These ``.md`` files are the same prompt definitions the Claude/Codex plugins expand, so
    returning the body gives Hermes faithful slash-command parity without shelling anything.
    """

    def handler(_args: str = "") -> str:
        return body

    return handler


# ── Comms notification hooks ────────────────────────────────────────────────────────────────


def _register_hooks(ctx) -> None:
    reg = getattr(ctx, "register_hook", None)
    inject = getattr(ctx, "inject_message", None)
    if not callable(reg) or not callable(inject):
        return

    def _inject(text):
        if not text:
            return
        # inject_message is documented as inject_message(content, role="user"); tolerate either arity.
        if not _safe_call(inject, text):
            _safe_call(inject, text, "user")

    def on_session_start(**_kwargs):
        _inject(_session_start_context(os.getcwd()))

    def pre_llm_call(**kwargs):
        sid = str(kwargs.get("task_id") or kwargs.get("session_id") or "default")
        _inject(_delta_context(os.getcwd(), sid))

    _safe_call(reg, "on_session_start", on_session_start)
    _safe_call(reg, "pre_llm_call", pre_llm_call)


def _session_start_context(cwd: str) -> str:
    """Boot context: the operating discipline, plus a condensed comms inbox if the broker responds."""
    messages = _inbox_messages(cwd, 8)
    if messages is None:  # broker unavailable — discipline only
        return _DISCIPLINE
    if messages:
        return (
            f"{_DISCIPLINE} {_COMMS_TOOLS}\n"
            "Recent messages (front-matter only; call message_get with an id to read a body):\n"
            f"{_format_lines(messages)}"
        )
    return f"{_DISCIPLINE} {_COMMS_TOOLS} No messages in your rooms yet — post one to kick things off."


def _delta_context(cwd: str, session_id: str) -> str | None:
    """Per-turn delta: inject only messages newer than this session's high-water mark."""
    messages = _inbox_messages(cwd, 30)
    if not messages:
        return None
    max_ts = max((_ts(m) for m in messages), default=0)
    hwm_file = _hwm_path(session_id)
    if not hwm_file.exists():  # first turn: baseline only, do not replay history
        _write_text(hwm_file, str(max_ts))
        return None
    hwm = _read_int(hwm_file)
    fresh = [m for m in messages if _ts(m) > hwm]
    if not fresh:
        return None
    _write_text(hwm_file, str(max_ts))
    return (
        "New basemind agent-comms message(s) since your last turn (front-matter only — call "
        "message_get with an id to read a body):\n"
        f"{_format_lines(fresh)}\n"
        "Reply with room_post {room, subject, body, reply_to:<id>} if a response is warranted."
    )


# ── basemind CLI bridge + helpers ───────────────────────────────────────────────────────────


def _inbox_messages(cwd: str, limit: int):
    """Return the inbox message list (possibly empty), or ``None`` when the broker is unavailable."""
    data = _run_basemind_json(["comms", "inbox", "--root", cwd, "--json", "--limit", str(limit)], timeout=6)
    if data is None:
        return None
    messages = data.get("messages")
    return messages if isinstance(messages, list) else []


def _run_basemind_json(args, timeout):
    try:
        proc = subprocess.run(  # noqa: S603,S607 - fixed argv, no shell
            ["basemind", *args],
            capture_output=True,
            text=True,
            timeout=timeout,
        )
    except Exception:
        return None
    if proc.returncode != 0 or not proc.stdout.strip():
        return None
    try:
        return json.loads(proc.stdout)
    except Exception:
        return None


def _format_lines(messages) -> str:
    return "\n".join(f"  • [{m.get('subject', '')}] from {m.get('from', '')} (id: {m.get('id', '')})" for m in messages)


def _ts(message) -> int:
    try:
        return int(message.get("ts_micros") or 0)
    except (TypeError, ValueError):
        return 0


def _hwm_path(session_id: str) -> Path:
    safe = "".join(c if (c.isalnum() or c in "._-") else "_" for c in session_id) or "default"
    return Path(tempfile.gettempdir()) / f"basemind-comms-hwm-{safe}"


def _front_matter_description(body: str) -> str | None:
    """Pull ``description:`` from a leading ``---`` YAML front-matter block, if present."""
    if not body.startswith("---"):
        return None
    end = body.find("\n---", 3)
    if end == -1:
        return None
    for line in body[3:end].splitlines():
        stripped = line.strip()
        if stripped.startswith("description:"):
            return stripped[len("description:") :].strip().strip("\"'") or None
    return None


def _safe_call(fn, *args) -> bool:
    """Call ``fn(*args)``; return True on success, False on any exception. Never raises."""
    try:
        fn(*args)
        return True
    except Exception:
        return False


def _read_text(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8", errors="replace")
    except Exception:
        return ""


def _write_text(path: Path, text: str) -> None:
    try:
        path.write_text(text, encoding="utf-8")
    except Exception:
        pass


def _read_int(path: Path) -> int:
    try:
        return int(path.read_text(encoding="utf-8").strip() or "0")
    except Exception:
        return 0
