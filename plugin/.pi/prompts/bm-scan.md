---
argument-hint:
    - path
description: Build or refresh the basemind index by running `basemind scan` via the CLI — works without the MCP server (use it when basemind reports "no index" / "no indexed files").
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:6c2f6f7c9aee594f8063387ce6b10e1e456b1ff382d303ac9594851baf989ca2
Source-Hash: blake3:db4c3979a26b54efd5467f96ce1fccaf62e7765db011aa2526d6e8ad411e694b
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
  the lock — use `basemind rescan [path…]` (forwards to the running daemon) or `admin { mode: "rescan" }` over MCP instead (lock errors exit 3).

## See also

The `basemind-scan` skill for the full workflow, binary-resolution order, and `extra_roots`
config for indexing directories outside the repo.
