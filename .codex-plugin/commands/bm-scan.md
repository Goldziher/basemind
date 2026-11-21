---
description: Build or refresh the basemind index by running `basemind scan` via the CLI — works without the MCP server (use it when basemind reports "no index" / "no indexed files").
argument-hint:
    - path
name: bm-scan
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:f988a2ac69c83ee231b252fa4a593f35fdfd327f37d25c5300650d35083918ad
Source-Hash: blake3:af86ee48bbc012fa865e3b106449e49b6122356c772ddf080e951fdece51097e
Schema-Version: v1
-->

# bm-scan — build or refresh the basemind index

Run `basemind scan` via the CLI so the code map exists and is current.

## When to use

basemind (or its statusline) reports "no index" / "no indexed files", an MCP tool returns empty
results that shouldn't be empty, or the index is stale after large changes.

## How to use

```sh
basemind scan                  # no argument
basemind rescan "$ARGUMENTS"   # a path argument
```

- No argument → `basemind scan`, a full working-tree scan.
- A path argument (`/bm-scan src/mcp`) → `basemind rescan src/mcp`, which re-indexes only that path
  (incremental) when a working-tree index exists; with none yet, or one that must be rebuilt, it
  runs a full scan instead. `scan` takes no path.
- Prefer the plugin-managed, version-matched binary: run `${CLAUDE_PLUGIN_ROOT}/scripts/mcp-launch.sh scan`
  (or `… rescan "$ARGUMENTS"`)
  (in other hosts, `scripts/mcp-launch.sh` in the plugin directory). Use bare `basemind` from `PATH`
  only when no plugin launcher exists, or build a dev binary with `cargo build --release` and use
  `./target/release/basemind`.

## Notes

- Report files scanned / updated / skipped and elapsed time. Non-extractable files are
  **skipped**, not failures.
- If a daemon is running, `scan` is forwarded to it and reports `scan via daemon`. Another
  process holding the store lock (`basemind watch`, another scan) makes `scan` exit 3 and name it.

## See also

The `basemind-scan` skill for the full workflow, binary-resolution order, and `extra_roots`
config for indexing directories outside the repo.
