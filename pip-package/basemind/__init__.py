"""
basemind: Code-map MCP server + scanner — content-addressed, Fjall-backed inverted index over tree-sitter outlines.
"""

__version__ = "0.20.0"

try:
    from .hermes import register  # noqa: F401
except Exception:  # pragma: no cover - defensive: CLI must import even if the plugin can't load
    pass
