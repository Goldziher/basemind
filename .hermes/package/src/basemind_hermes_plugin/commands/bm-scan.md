---
name: bm-scan
description: Build or refresh the basemind index by running `basemind scan` via the CLI — works without the MCP server (use it when basemind reports "no index" / "no indexed files").
argument-hint: [path]
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:54d00ed56e2003136b06bcf59460a2782258f8e2e35475b07b65c2adcb5e873c
Source-Hash: blake3:ca37d0bd1accfd4a3156158a1f47fd4568c00994d83f95330f41124a857f34dc
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
- If `basemind` isn't on `PATH`: use the plugin-managed cache
  (`${XDG_CACHE_HOME:-~/.cache}/basemind/bin/<version>/basemind`), or build a dev binary with
  `cargo build --release` and use `./target/release/basemind`.

## Notes

- Report files scanned / updated / skipped and elapsed time. Non-extractable files are
  **skipped**, not failures.
- If a `basemind serve` MCP server already holds the store lock for this repo, `scan` errors on
  the lock — use the `rescan` MCP tool instead, or stop the server first.

## See also

The `basemind-scan` skill for the full workflow, binary-resolution order, and `extra_roots`
config for indexing directories outside the repo.
