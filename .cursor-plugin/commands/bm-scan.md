---
description: Build or refresh the basemind index by running `basemind scan` via the CLI — works without the MCP server (use it when basemind reports "no index" / "no indexed files").
argument-hint:
    - path
name: bm-scan
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:20f7195b9b4ed9e2e3be8053661a622d24b7f00ddb5f1d81eb9848b5a0a751cc
Source-Hash: blake3:f893b90330756fe7bd43b98966ed88c6e2116dfd9e8e37d995c2c64aaa745875
Schema-Version: v1
-->

# bm-scan — build or refresh the basemind index

Run `basemind scan` via the CLI so the code map exists and is current.

## When to use

basemind (or its statusline) reports "no index" / "no indexed files", an MCP tool returns empty
results that shouldn't be empty, or the index is stale after large changes.

## How to use

```sh
basemind scan ${ARGUMENTS:-}
```

- No argument → full working-tree scan.
- A path argument (`/bm-scan src/mcp`) → scope the scan to that path (incremental).
- Prefer the plugin-managed, version-matched binary: run `${CLAUDE_PLUGIN_ROOT}/scripts/mcp-launch.sh scan ${ARGUMENTS:-}`
  (in other hosts, `scripts/mcp-launch.sh` in the plugin directory). Use bare `basemind` from `PATH`
  only when no plugin launcher exists, or build a dev binary with `cargo build --release` and use
  `./target/release/basemind`.

## Notes

- Report files scanned / updated / skipped and elapsed time. Non-extractable files are
  **skipped**, not failures.
- If the basemind daemon is running, `scan` is forwarded to it and reports `scan via daemon`. Another
  process holding the store lock (`basemind watch`, another scan) makes `scan` exit 3 and name it.

## See also

The `basemind-scan` skill for the full workflow, binary-resolution order, and `extra_roots`
config for indexing directories outside the repo.
