---
name: basemind-scan
description: >-
  Build or refresh the basemind index by running `basemind scan` via the CLI. Use this when
  basemind reports "no index" / "no indexed files", when the MCP server isn't available, or
  after large changes when the index is stale. Works without the MCP server — it shells the
  basemind binary directly.
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:6a2da891a9f4c927e817d6507b8118afe9d9140a9473177ead3e43fae3ae468f
Source-Hash: blake3:36dad38afc8a716a210a7f724cc29942cc9ef1d96e5f9ea2e9bc8e4cb2eeb8b7
Schema-Version: v1
-->

# basemind-scan — build or refresh the index (no MCP server required)

basemind answers code-map questions from an index in a machine-global cache (Linux
`~/.local/share/basemind/`, macOS `~/Library/Application Support/basemind/`; override
`BASEMIND_DATA_HOME`), keyed by workspace. That index is built by
`basemind scan`. This skill runs the scan via the **CLI**, so it works even when the MCP server
(`basemind serve`) is not running — which is exactly the situation when basemind reports
**"no index"** or **"no indexed files"**, or when MCP tools aren't loaded in the session.

## When to use

- basemind / the statusline reports **no index** for this repo.
- An MCP tool returns "no indexed files" or empty results that should not be empty.
- The MCP server isn't running or isn't reachable, and you need the code map.
- The index is stale after large changes and you want a full rebuild.

## How to run

From the repository root:

```sh
basemind scan            # full working-tree scan
basemind scan <path>     # scope to a path (incremental)
```

Finding the binary (in order of preference):

1. `basemind` on `PATH`.
2. The plugin-managed binary the MCP launcher caches:
   `${XDG_CACHE_HOME:-~/.cache}/basemind/bin/<version>/basemind`.
3. A dev build: `cargo build --release` then `./target/release/basemind scan`.

## Notes

- The scan writes the content-addressed blob store + Fjall inverted index into the machine-global
  cache (never inside the repo). Seconds for small repos; ~22 s for an ~80k-file TypeScript monorepo.
- The code map holds code only. Markdown, JSON, YAML, TOML, XML, CSV, INI and similar prose/config/data
  files go to the document tier instead (chunked, searchable with `memory` mode `documents`; they
  never appear in `symbols`, `outline` or `grep`; `files` and `find` list them). Anything that is neither code nor an
  extractable document is **skipped**, not counted as a failure. Existing indexes migrate on the
  next scan, which also reclaims stale blobs (`cleanup: reclaimed N orphaned blob(s)`).
- If a `basemind serve` MCP server is already running for this repo it holds the store lock, so a
  CLI `scan` will fail with a lock error. Use `basemind rescan [path…]` (the CLI forwards to the running daemon) or
  `admin { mode: "rescan" }` over MCP instead; the lock error exits with code 3.
- **Indexing directories outside the repo** — set `scan.extra_roots` in the repo-root `basemind.toml`
  to a list of absolute paths (e.g. a Bazel external repo cache) to index them alongside the repo.
  This needs `BASEMIND_ALLOW_EXTRA_ROOTS` in the environment that launches basemind (`1`, or a
  `:`-separated list of absolute workspace roots to grant only those): the config file lives inside
  the repository, so without that operator opt-in the entries are ignored with a warning. A
  filesystem or volume root and credential directories (`.ssh`, `.aws`, `.gnupg`, `/etc`) are
  refused either way.
  Their files are keyed by absolute path (so results for them are absolute, not repo-relative) and
  are (re-)indexed on a full `scan` only — the live watcher does not track them. Git tools (blame)
  don't apply to external files; the code map (symbols / references / outlines) and document search
  do.
- Credential and key files (`.env*`, `.aws/`, `.ssh/`, `.npmrc`, `*.pem`, `*.key`, ...) are never
  indexed. `[scan] floor_allow` opts a class back in (`.env.*` also matches `.env.example`), but a
  credential entry needs `BASEMIND_ALLOW_REPO_CREDENTIALS` in the environment that launches
  basemind — a repository's own file cannot un-exclude your secrets.
  `[languages.<grammar>]` re-maps or disables a misdetected grammar.
- After a successful scan, both the MCP tools and `basemind code …` have a fresh index.
- The CLI shares the exact same machine-global cache as the MCP server — see the `basemind-cli`
  skill for the full query surface, or `basemind-code-search` / `basemind-git-history` /
  `basemind-documents` for the per-capability workflows.
