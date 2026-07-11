"""basemind-hermes-plugin: Hermes Agent plugin for basemind (skills, slash commands, comms hooks)."""

__version__ = "0.21.0"

try:
    from .hermes import register  # noqa: F401
except Exception:  # pragma: no cover - defensive: never break plugin load
    pass
