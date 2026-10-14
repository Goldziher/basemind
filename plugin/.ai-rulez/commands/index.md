# Concepts

* [Bm Doctor](bm-doctor.md) - Diagnose and recover basemind when it isn't working (MCP tools missing/erroring, "no index", dead server) — runs CLI checks and gives the client-specific way to reconnect the server.
* [Bm Init](bm-init.md) - Onboard (or refresh) basemind in this repo — write basemind.toml and inject a "prefer basemind over grep/read/git" rules block into a rules file you choose (CLAUDE.local.md / AGENTS.local.md / CLAUDE.md / AGENTS.md / ai-rulez).
* [Bm Scan](bm-scan.md) - Build or refresh the basemind index by running `basemind scan` via the CLI — works without the MCP server (use it when basemind reports "no index" / "no indexed files").
* [Bm Stats](bm-stats.md) - Show the basemind dashboard — resource footprint (disk + RAM) and activity (tool calls, per-tool histogram, estimated tokens saved). Works with or without the MCP server.
* [Bm Statusline](bm-statusline.md) - Enable the basemind status line in your Claude Code user settings (one-time setup).
* [Bm](bm.md) - Ask basemind anything about the current codebase — outlines, refs, callers, git history, blame, diffs, docs, memory.
