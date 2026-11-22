---
description: Build or refresh the basemind index by running `basemind scan` via the CLI. Use this when basemind reports "no index" / "no indexed files", or when `index_status` reports `degraded` or `rescan_needed`. Works without the MCP server — it shells the basemind binary directly.
name: basemind-scan
license: MIT
metadata:
    author: Goldziher
    repository: https://github.com/Goldziher/basemind
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:fa2b9f49ce3d009ebfecf51f1748703195fde17ec79c0de5ab39afb36097e508
Source-Hash: blake3:7cf46d1e1bc5b1e7e3626621108e4cc0491f3257da1d78e8c21cb3a3c193b15b
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
- An MCP tool returns "no indexed files".
- The MCP server isn't running or isn't reachable and the index is missing or unhealthy. A healthy
  cached index still answers CLI queries (`basemind code …`) without a scan.
- `index_status` reports `degraded` or `rescan_needed: true` and you want a full rebuild. Either one
  qualifies: a scan that stops after writing the code map sets `degraded` alone.
- Files under `scan.extra_roots` changed: only a full scan re-indexes them (`rescan <path>` skips
  them). With a live `basemind serve` holding the lock, `index_rescan {}` is the full pass; with a
  standalone `basemind watch`, stop it, run `basemind scan`, then restart it.

Not for a healthy index that misses a file you just created: run `basemind rescan <path>` (or
`index_rescan {"paths":[…]}`) for that file. A running server already re-indexes the files you edit
in the repository; it does not watch `scan.extra_roots`.

## How to run

From the repository root:

```sh
basemind scan                   # full working-tree scan
basemind rescan <path>          # re-index only these paths (incremental)
```

Finding the binary (in order of preference). The plugin runs its own version-matched binary, so
use it first: a bare `basemind` from `PATH` can be a different version over the same cache.

1. The plugin launcher, which installs and execs the version-matched binary:
   `${CLAUDE_PLUGIN_ROOT}/scripts/mcp-launch.sh scan` (in other hosts, `scripts/mcp-launch.sh` in the
   plugin directory). Its binary is cached at `${XDG_CACHE_HOME:-~/.cache}/basemind/versions/<version>/basemind`.
2. Fallback when no plugin launcher exists: `basemind` on `PATH`.
3. A dev build: `cargo build --release` then `./target/release/basemind scan`.

## Notes

- The scan writes the content-addressed blob store + Fjall inverted index into the machine-global
  cache (never inside the repo). Seconds for small repos; ~22 s for an ~80k-file TypeScript monorepo.
- The code map (outline, symbols, refs) covers code only. Markdown, JSON, YAML, TOML, XML, CSV, INI
  and similar prose/config/data files go to the document tier instead: chunked for `docs_search`
  (search by meaning), never in `code_symbols` or `code_outline`, but `code_grep` finds exact text
  in them and `find_files` lists them. Anything that is neither code nor an
  extractable document is **skipped**, not counted as a failure. Existing indexes migrate on the
  next scan, which also reclaims stale blobs (`cleanup: reclaimed N orphaned blob(s)`).
- If a daemon is running, `scan` is forwarded to it and prints `scan via daemon`; it never
  starts one. A flag the daemon cannot apply (`--rebuild-git-history`, a `--documents-*` /
  `--llm-*` override) exits 2 naming it. Another process holding the store lock
  (`basemind watch`, another scan) makes `scan` exit 3 and name the holder; stop a standalone
  watcher, scan, then restart it.
- **Indexing directories outside the repo** — set `scan.extra_roots` in the repo-root `basemind.toml`
  to a list of absolute paths (e.g. a Bazel external repo cache) to index them alongside the repo.
  This needs `BASEMIND_ALLOW_EXTRA_ROOTS` in the environment that launches basemind (`1`, or a
  `:`-separated list of absolute workspace roots to grant only those): the config file lives inside
  the repository, so without that operator opt-in the entries are ignored with a warning. A
  filesystem or volume root and credential directories (`.ssh`, `.aws`, `.gnupg`, `/etc`) are
  refused either way.
  Their files are keyed by absolute path (so results for them are absolute, not repo-relative) and
  are (re-)indexed on a full `scan` only — the live watcher does not track them. Git tools (blame)
  don't apply to external files; the code map (symbols / usages / outlines) and document search
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
