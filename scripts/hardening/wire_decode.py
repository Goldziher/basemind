"""Read a basemind MCP text answer back to the JSON the tool built (ADR-0053).

The text block is compact JSON, TOON (`code_outline`, `git_diff`, a `find_files` list), or for
`code_grep` TOON lines followed by rows under `# <path>` / `## <symbol>` headers. Standard library
only; mirrors `src/wire.rs`. Covers the TOON subset the encoder emits: nested objects, inline
arrays, tabular arrays and `- ` list items.
"""

from __future__ import annotations

import json
import re
from typing import Any

FILE_HEADER = "# "
SYMBOL_HEADER = "##"
HITS = "hits"
INDENT = 2
KEY_RE = re.compile(r'^("(?:[^"\\]|\\.)*"|[^\[\]{}:"]+?)(?:\[(\d+)\](?:\{(.*)\})?)?:(?: (.*))?$')


class WireError(ValueError):
    """The text is none of the shapes a basemind answer takes."""


def decode(text: str) -> Any:
    """The JSON `text` encodes; raises `WireError` when it is no known shape."""
    trimmed = text.lstrip()
    if trimmed.startswith("{"):
        try:
            return json.loads(trimmed)
        except json.JSONDecodeError:
            pass
    lines = text.split("\n")
    start = next((i for i, line in enumerate(lines) if line.startswith(FILE_HEADER)), None)
    head = lines if start is None else lines[:start]
    head = [line for line in head if line.strip()]
    value: dict[str, Any] = _object(head, 0, 0)[0] if head else {}
    if start is not None:
        if HITS in value:
            raise WireError("`hits` is both in the TOON lines and in the row block")
        value[HITS] = _grep_rows(lines[start:])
    return value


def _unquote(raw: str) -> str:
    if raw.startswith('"'):
        try:
            return json.loads(raw)
        except json.JSONDecodeError as exc:
            raise WireError(f"bad quoted string {raw!r}: {exc}") from exc
    return raw


def _grep_rows(lines: list[str]) -> dict[str, Any]:
    files: dict[str, dict[str, list[str]]] = {}
    groups: dict[str, list[str]] | None = None
    group = ""
    for line in lines:
        if line.startswith(FILE_HEADER):
            groups = files.setdefault(_unquote(line[len(FILE_HEADER) :]), {})
            group = ""
        elif groups is None:
            raise WireError(f"row before any file header: {line!r}")
        elif line == SYMBOL_HEADER:
            group = ""
            groups.setdefault(group, [])
        elif line.startswith(SYMBOL_HEADER + " "):
            group = _unquote(line[len(SYMBOL_HEADER) + 1 :])
            groups.setdefault(group, [])
        elif line[:1].isdigit():
            groups.setdefault(group, []).append(line)
        else:
            raise WireError(f"not a header or a numbered row: {line!r}")
    return files


def _cells(raw: str) -> list[str]:
    cells: list[str] = []
    current = ""
    quoted = False
    escaped = False
    for ch in raw:
        if quoted:
            current += ch
            if escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == '"':
                quoted = False
        elif ch == '"':
            quoted = True
            current += ch
        elif ch == ",":
            cells.append(current)
            current = ""
        else:
            current += ch
    cells.append(current)
    return cells


def _scalar(raw: str) -> Any:
    raw = raw.strip()
    if raw.startswith('"'):
        return _unquote(raw)
    if raw in ("true", "false", "null"):
        return json.loads(raw)
    if re.fullmatch(r"-?\d+(\.\d+)?([eE][+-]?\d+)?", raw):
        return json.loads(raw)
    return raw


def _indent(line: str) -> int:
    return len(line) - len(line.lstrip(" "))


def _object(lines: list[str], i: int, indent: int) -> tuple[dict[str, Any], int]:
    out: dict[str, Any] = {}
    while i < len(lines) and _indent(lines[i]) == indent:
        match = KEY_RE.match(lines[i].strip())
        if match is None:
            raise WireError(f"not a TOON line: {lines[i]!r}")
        key, count, fields, rest = match.groups()
        key = _unquote(key)
        i += 1
        if count is None:
            if rest is None:
                out[key], i = _object(lines, i, indent + INDENT)
            else:
                out[key] = _scalar(rest)
        elif fields is not None:
            names = [_unquote(f.strip()) for f in _cells(fields)]
            rows = lines[i : i + int(count)]
            out[key] = [dict(zip(names, map(_scalar, _cells(row.strip())), strict=False)) for row in rows]
            i += int(count)
        elif rest:
            out[key] = [_scalar(c) for c in _cells(rest)]
        else:
            out[key], i = _list_items(lines, i, indent + INDENT, int(count))
    return out, i


def _list_items(lines: list[str], i: int, indent: int, count: int) -> tuple[list[Any], int]:
    items: list[Any] = []
    for _ in range(count):
        if i >= len(lines) or not lines[i].strip().startswith("-"):
            raise WireError(f"expected a list item at {lines[i : i + 1]!r}")
        body = lines[i].strip()[1:].strip()
        if KEY_RE.match(body) and not body.startswith('"'):
            lines[i] = " " * (indent + INDENT) + body
            item, i = _object(lines, i, indent + INDENT)
            items.append(item)
        else:
            items.append(_scalar(body))
            i += 1
    return items, i
