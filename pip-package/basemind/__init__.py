"""
basemind: Code-map MCP server + scanner — content-addressed, Fjall-backed inverted index over tree-sitter outlines.
"""

__version__ = "0.18.1"

# Hermes Agent plugin entry point (group `hermes_agent.plugins`, target = this package).
# Hermes imports the package and calls `basemind.register(ctx)`. Guarded so a plugin-load
# failure can never break the `basemind` CLI, which imports this same package.
try:
    from .hermes import register  # noqa: F401
except Exception:  # pragma: no cover - defensive: CLI must import even if the plugin can't load
    pass
