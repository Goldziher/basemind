---
argument-hint:
    - path
description: Build or refresh the basemind index by running `basemind scan` via the CLI — works without the MCP server (use it when basemind reports "no index" / "no indexed files").
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:e69a233418e2678e1a9d84d48991ca59046bcc3ae1bb9b57c795a0bc684bcf40
Source-Hash: blake3:0a6ab7c9cd207021339785c703ecf23c6ae242ff0aea1b92599c66f1990e6dc5
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
- If a daemon is running, `scan` is forwarded to it and reports `scan via daemon`. Another
  process holding the store lock (`basemind watch`, another scan) makes `scan` exit 3 and name it.

## See also

The `basemind-scan` skill for the full workflow, binary-resolution order, and `extra_roots`
config for indexing directories outside the repo.
