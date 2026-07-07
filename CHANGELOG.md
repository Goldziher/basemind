# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

<!-- Keep a Changelog repeats Added/Changed/Fixed headings per version. -->
<!-- markdownlint-disable MD024 -->

## [Unreleased]

## [0.19.0] — 2026-07-07

> Minor release — the index + blob schema version bumps (`RELEASE_MINOR` 18 → 19), so `.basemind/`
> is wiped and rebuilt on the next scan.

### Added

- **Hermes Agent plugin.** basemind now integrates with [Hermes Agent](https://hermes-agent.nousresearch.com):
  `pip install basemind` exposes a Hermes plugin (discovered via the `hermes_agent.plugins` entry
  point) that bundles the helper skills, slash commands, and agent-comms notifications. Tools are
  wired through Hermes's MCP config (`mcp_servers.basemind` in `~/.hermes/config.yaml`) — a Hermes
  plugin cannot declare an MCP server. See the README "Hermes" install section. Closes #36.

### Changed

- **Claude status line survives version bumps.** `/bm-statusline` now writes a version-independent
  resolver as the `statusLine` command (re-resolving the newest installed `statusline.sh` at each
  render) instead of a version-pinned path that broke on update. The bar also shows the running
  basemind version (`v<version>`, full/compact tiers; opt out with `BASEMIND_STATUSLINE_VERSION=0`).

### Performance

Wide hot-path allocation + algorithmic sweep across the scanner, extraction, git, and MCP query
paths. All changes are internal — no behavior, response-shape, or on-disk-format change, and the
determinism assertions are unchanged.

- **`call_graph`** (`bfs_callees`) precomputes a name→sites map once instead of re-scanning every
  indexed symbol for each discovered callee — O(max_nodes × symbols) → O(symbols + max_nodes).
- **`architecture_map`**: `callee_counts` no longer allocates a `String` on every one of up to 4M
  scanned call sites (allocates only on first sight of a name); the symbol-tier fan-out set skips the
  allocation for duplicate callees; PageRank reuses a single accumulator buffer (20 allocations → 1).
  Output stays byte-identical.
- **Tree-sitter `QueryCursor`** is now pooled per thread alongside the parser pool — ~3 fewer
  allocations per scanned file.
- **git**: `diff_file` no longer clones the (potentially multi-MB) file buffers just to record
  existence; the `blame` / `log` / `commit-files` disk-cache writers serialize from borrowed slices
  instead of cloning the whole payload; `dependents` drops a `RelPath`↔`PathBuf` round-trip over the
  entire file index.
- **document tier**: `build_doc_rows` moves per-chunk text + embedding vectors instead of cloning
  them; the archive-extension check is an `AHashSet` (O(1)) instead of a 40-element linear scan.
- **paths**: the scanner (`scan_paths`) and watcher skip a `\`→`/` `String` allocation on Unix.
- **BM25 index**: the posting value drops a `.to_vec()` (`lsm_tree::Slice: From<[u8; N]>`) and the
  forward-map value is built in one sized allocation instead of an alloc-plus-realloc.

### Internal

- `for_each_call_in_file` (the dual-backend Fjall / in-RAM call-site scan) now lives once in
  `mcp/helpers_calls.rs`, shared by `architecture_map` and the call-graph helpers; `capture_name` is
  deduplicated into `extract/mod.rs`. Removed a dead `last_emitted_key` in `find_implementations`.

## [0.18.1] — 2026-07-06

> **Patch release — blob + index format unchanged.** `RELEASE_MINOR` stays 18; no `.basemind/`
> rebuild. Query-side only.

### Changed

- **`architecture_map` symbol tier now ranks by specificity-weighted fan-in.** The repo-wide
  symbol tier previously ranked by raw name-based call count, so ubiquitous names (`new`, `from`,
  `default`) dominated — on a real repo the top ~60 hubs were all `new`, which misleads an agent
  routing on the result. Fan-in is now divided by the number of files defining that name
  (`RepoGraph::def_counts`), so a genuine single-definition hub outranks a name that is merely
  everywhere. The raw `fan_in` count is still reported verbatim on each node.

### Fixed

- **`architecture_map` symbol-tier `score` is now monotonic with node order.** `fan_out` was
  computed _after_ the knee-cut, so the blended `score` never actually influenced selection or
  ordering and the emitted nodes were not sorted by their own `score`. Selection, knee-cut, and
  `score` now all key off the single specificity-weighted signal, matching the module/file tiers;
  `fan_out` and per-file churn remain reported but no longer pretend to affect the ranking.

## [0.18.0] — 2026-07-06

> **Minor release — cache rebuild.** `RELEASE_MINOR` bumps to 18, so both the Fjall index and the
> content-addressed blob store are wiped and rebuilt from source on the next `basemind scan` (the
> standard minor-release migration). No config or on-wire tool-response change.

### Added

- **`architecture_map` MCP tool — deterministic, LLM-free architecture overview.** Ranks modules
  and symbols by graph centrality + churn and surfaces circular-dependency clusters, all derived
  from real call edges (never hallucinated). A whole-repo adjacency graph is built in memory at
  query time from the cached call sites (Fjall `calls_by_path`, or the in-RAM call cache in
  read-only multi-session mode) — no new Fjall partition, no blob I/O. Metrics are cheap and
  deterministic: fan-in/fan-out degree, fixed-iteration PageRank (damping 0.85, 20 iterations,
  id-ordered accumulation — call-twice byte-identical), and iterative Tarjan SCC for cycle
  clusters. Three tiers via `granularity`: `module` (default; files collapsed to a directory
  prefix at `depth`), `file` (the base graph), and `symbol` (function→function drill-down under a
  `focus`). Results are ranked, Kneedle knee-cut, then bounded by the shared `apply_budget` token
  budgeter; edges are emitted only between surviving nodes. Optional `include_churn` overlay
  reuses the `hot_files` aggregation. Node/edge caps set `truncated` + `truncation_reason` on
  overflow. CLI: `query architecture-map`. New internal `kneedle` knee-detection helper
  (Satopaa 2011, dependency-free).

## [0.17.0] — 2026-07-05

> **Minor release — cache rebuild.** `RELEASE_MINOR` bumps to 17, so both the Fjall index and the
> content-addressed blob store are wiped and rebuilt from source on the next `basemind scan` (the
> standard minor-release migration). No config or on-wire tool-response change.

### Added

- **Semantic code search (Phase 1, vector-only).** New `search_code` MCP tool runs vector KNN over
  source-code chunks and `get_chunk` fetches a chunk's body — the same two-call pointer pattern as
  `search_symbols` → `expand`. Chunks are derived from the cached L1/L2 extraction + source bytes
  (no re-parse): one chunk per symbol span plus module-level gap chunks, oversized spans split with
  overlap. A content-addressed `<hash>.chunk.msgpack` sidecar caches chunks + embeddings so an
  unchanged file skips re-embedding. Vectors land in a new LanceDB `code_chunks` table alongside the
  documents/memory tables (shared embedding preset). CLI: `query search-code` / `query get-chunk`.
  Behind the `code-search` cargo feature (folded into `full`); BM25 keyword + RRF hybrid fusion land
  in later phases. The `code_chunks` table is created lazily on open, so it rides the existing
  minor-release `.basemind/` rebuild — no separate schema bump.
- **Keyword code search (Phase 2, native BM25).** `search_code` gains a `mode` parameter:
  `semantic` (the default vector lane) or `keyword`, a native Okapi BM25 (`k1 = 1.2`, `b = 0.75`)
  lane over each chunk's symbol + signature + doc + body text. Postings live in two new Fjall
  keyspaces (`code_bm25_postings` inverted + `code_bm25_by_path` forward for O(prefix) re-scan
  deletes); term frequency and document length are inlined so scoring a term is a single prefix
  scan, and corpus stats (`N`, `avgdl`) are recomputed once per scan. The keyword lane reads only
  Fjall and the chunk sidecar — no LanceDB and no embedder — so it works even with
  `[code_search] embed = false`. RRF fusion of the two lanes plus an exact symbol lane lands in
  Phase 3.
- **Hybrid code search (Phase 3, RRF fusion + exact lane + rerank).** `search_code` gains a third
  `mode`, `hybrid` — now the **default** — that fuses three lanes via Reciprocal Rank Fusion: the
  vector (semantic) lane, the BM25 keyword lane, and a new **exact symbol lane** that resolves an
  identifier-shaped query against the `symbols_by_name` index to the chunks defining that symbol
  (the scope-aware signal a pure vector+keyword stack can't produce). Fusion joins the lanes on
  `chunk_id` and is score-scale-agnostic, so it blends an L2 distance, a BM25 score, and a symbol
  match order without normalization; it degrades gracefully, dropping any lane that is unavailable
  (e.g. the vector lane without embeddings). An optional cross-encoder **rerank** pass (`rerank:true`
  / `[code_search.reranker]`, off by default) reuses the same xberg reranker as the documents tier.
  No index-schema change — reuses the existing keyspaces and the `code_chunks` table.
- **`search_code` why-matched provenance.** Hybrid hits now carry per-lane provenance:
  `matched_lanes` (which of `exact` / `vector` / `keyword` produced the hit, in fixed lane order)
  plus the 1-based `exact_rank` / `vector_rank` / `keyword_rank` the chunk held in each contributing
  lane. Lets an agent tell an exact-symbol match from a semantic neighbor or a cross-lane agreement
  without a second call. Additive, non-breaking response fields (absent outside hybrid mode).
- **`status` reports boot-scan indexing state.** When `serve` auto-scans an empty index on startup,
  that build cost was invisible and could fold into the first query's latency. `status` now carries
  `indexing: true` while the boot scan runs and `index_build_ms` once it completes — so a client can
  tell "index not ready yet, poll again" from "no matches", separating index-build time from query
  time. Additive fields, absent on the common ready path.
- **Code-intelligence tier: scope- and import-resolved navigation.** A post-scan resolve pass links
  each reference to its actual definition — scope-aware, not a name match. Intra-file resolution runs
  across 80+ languages via tree-sitter `locals` (with vendored queries for Python / TypeScript / TSX
  / Go); JS/TS additionally get cross-file resolution via oxc scope analysis + `oxc_resolver` import
  stitching. New `goto_definition` MCP tool + `query goto-definition` CLI resolve a `path:line:column`
  to its definition, following cross-file import bindings. Behind the `code-intel-js` feature (folded
  into `full`) for the JS/TS engine; the `locals` tier is always on.
- **Document tier reuses the content-addressed cache instead of re-embedding every scan.** The
  documents pipeline now tracks each file in a new `Index.doc_files` map and, before extracting,
  reads back the cached `<hash>.doc.msgpack` blob (which already carries chunks + embeddings): an
  unchanged doc is skipped, and byte-identical content at any other path — or, with the shared cache
  below, any other worktree — reuses the extraction instead of re-running xberg + ONNX. Removed docs
  now have their LanceDB rows and tracking entry pruned. `doc_files` is `#[serde(default)]` — additive,
  no schema bump.
- **Shared blob cache across git worktrees.** Linked worktrees now resolve their content-addressed
  blob directory to the main worktree's `.basemind/blobs` (via gix `common_dir`), so a file's
  extraction + embedding is computed once and shared across every worktree of the clone; views +
  LanceDB stay per-worktree. Auto-GC is disabled while a shared cache exists (a single-worktree sweep
  could reap a sibling's blobs).
- **Bounded embedding threads.** All ONNX embedding now runs on a dedicated rayon pool capped at
  `documents.embed_max_threads` (default `0` = auto `max(2, cores/4)`), and xberg's internal fan-out
  is capped to match. Code-map extraction keeps the full pool; embedding can no longer pin every core
  or balloon RSS on a large monorepo.
- **New config:** `documents.max_chunks_per_document` (default 2000) caps vector rows per document;
  `documents.extension_denylist` extends the built-in archive/binary skip list; `documents.embed_max_threads`
  bounds the embedding pool. `[scan] exclude` gains Bazel defaults (`bazel-out/`, `bazel-bin/`,
  `bazel-testlogs/`, `bazel-*/`), and a large-candidate-count scan logs a warning.
- **mtime+size fast-path on rescan.** An unchanged working-tree file is now confirmed with a single
  `stat()` (size + nanosecond mtime match + sidecars present) instead of a full read + blake3 hash —
  the bulk of the per-file cost on a large-monorepo warm rescan. mtime moved to nanosecond resolution
  so the fast-path is effectively race-free; it's an internal comparison value, so no schema bump.

### Changed

- **Serve boot defers embedding to a background pass.** On `serve` auto-scan of an empty index, the
  first pass now runs in a new `EmbedMode::Deferred` mode: it writes the code-map, the BM25 keyword
  lane, and the content-addressed blobs but **skips** the ONNX embedding, so `outline` /
  `search_symbols` / keyword `search_code` are queryable almost immediately instead of waiting on the
  embed of every file. A detached second pass then re-scans in `EmbedMode::Inline` to fill the vectors
  in (reusing the fast pass' content-addressed caches so only not-yet-embedded content is embedded,
  bounded by `embed.max_threads`), followed by GC. The CLI `basemind scan`, the watcher, and manual
  `rescan` stay `Inline` so scripted/CI use remains deterministic. No schema or config change.
- **Index schema revision — new `refs_by_def` / `refs_by_path` Fjall partitions** back the resolved
  edges; per-file resolution facts persist as content-addressed `<hash>.rref.msgpack` blobs. This
  ships as a minor-release cut: `RELEASE_MINOR` bumps at release time, wiping and rebuilding every
  `.basemind/` index + blob store on the next `basemind scan` (the resolve pass repopulates the new
  partitions). The `code_bm25_postings` / `code_bm25_by_path` partitions for the BM25 keyword lane
  (`INDEX_PARTITION_REVISION` → 4) ride the same index rebuild — the blobs are unchanged, so
  re-embedding is skipped and only the secondary index repopulates.

### Fixed

- **Code-search BM25 postings survive an embedder failure.** With `[code_search] embed = true`, a
  failed embedder load / inference / vector-count mismatch previously returned an error for the whole
  file, dropping its BM25 postings too — so a transient embedding fault silently punched keyword-lane
  holes. The scan now degrades to a keyword-only batch (chunks + BM25, no LanceDB rows, no embedded
  sidecar) and retries embedding on the next scan. BM25 is independent of embeddings and is indexed
  as such.
- **L1 extraction no longer drops (or crashes on) symbols in TSLP-fallback languages whose
  `tags.scm` leads a definition with an auxiliary capture.** The combined-L1 dispatch keyed on the
  match's _first_ capture, but adapted upstream tag queries (e.g. Ruby) prepend a `@doc` capture for
  a preceding comment — so a commented `def` classified as `Other`, panicking debug builds
  (`debug_assert!`) and silently dropping the symbol in release. A single such file could abort the
  whole parallel scan, leaving the index empty. Dispatch now scans to the first capture that
  classifies to a known L1 class, recovering those symbols across every fallback grammar.
- **`rescan` rejects path parameters that escape the repository root.** The `rescan` tool took raw
  path strings and joined them straight onto the repo root, so with `scan.respect_gitignore = false`
  a `paths: ["../../etc/passwd"]` traversal would read (and index) a file outside the repo, then be
  retrievable via `workspace_grep`. Request paths now pass through the same `normalize_query_path`
  root-boundary guard as `shell_spawn`; an escaping path is rejected with `invalid_params` instead of
  being scanned. Default `respect_gitignore = true` already blocked it structurally.
- **Archives and binaries are no longer unpacked + embedded during a scan.** `should_extract_document`
  rejected nothing by default, so xberg recursively unpacked `.zip/.tar/.jar/.whl/...` and embedded
  every entry, and binary blobs (`.so/.wasm/.class/...`) were embedded to no purpose — a large,
  pointless cost that pinned ~11 cores and grew `.basemind` to gigabytes on a monorepo. A built-in
  extension + MIME denylist now skips them before any xberg work (images stay allowed for OCR).
- **Document blobs are no longer reaped by the blob GC.** Because docs weren't tracked in the index,
  `collect_referenced_hashes` never marked their `.doc.msgpack` blobs live, so the boot/background GC
  deleted the entire doc cache after every scan. Doc-tier hashes are now unioned into the live set.
- **Full scans of large repos no longer abort with a stack overflow.** Tree-sitter parse trees and the
  msgpack (de)serialization of large extraction blobs can recurse past rayon's small default worker
  stack (~2 MiB) on pathological (deeply-nested / machine-generated) files, hard-aborting the process
  on a first full scan (e.g. a freshly-checked-out git worktree; a warm rescan hid it by skipping
  unchanged files). Extraction and the resolve pass now run on a dedicated rayon pool with a 256 MiB
  per-worker stack (reserved lazily, so it costs nothing on the common shallow path).

## [0.16.0] — 2026-07-02

Minor release: `RELEASE_MINOR` bumps 15 → 16, so every `.basemind/` index + blob store (including
`git-history.fjall/`) is wiped and rebuilt on the next `basemind scan`.

### Added

- **Resource footprint in `cache_stats` (disk + RAM).** The `cache_stats` MCP tool and
  `basemind cache stats` CLI now report a `total_bytes` that reconciles with `du` exactly, a
  per-component breakdown that **includes the `git-history.fjall/` index** (previously uncounted —
  the "1.15 GB reported vs 1.5 GB on disk" gap), an `other_bytes` catch-all so an uncounted
  directory can never silently vanish again, and process memory (`rss_bytes` + `peak_rss_bytes`) via
  a new cross-platform RSS reader (`src/sysres.rs`; mach on macOS, `/proc` + `getrusage` on Linux).
  Disk sizing now uses on-disk block counts, so Fjall's sparse journals are no longer over-reported.
  The Claude statusline gains the on-disk total + serve-process RSS.
- **Full CLI↔MCP parity.** Every MCP tool an agent can call is now reachable from the CLI: new
  `git search` (→ `search_git_history`), `governance audit` (→ `memory_audit`), `query expand`
  (→ `expand`), and a new `shells` group (`spawn`/`send`/`capture`/`kill`/`broadcast`/`list`
  → the `shell_*` tools, `--features shells`; sessions persist via the shared rmux daemon). Three
  CLI-only text primitives gained MCP tools — `delta`, `checkpoint`, `detect_waste`. A new
  `tests/cli_parity.rs` guard enumerates the advertised MCP tool set and fails if any tool lacks a
  CLI counterpart.
- **`scan.extra_roots` — index directories outside the repository root** ([#34]). Point the new
  `[scan] extra_roots = [...]` config knob at absolute paths outside the repo (e.g. a Bazel
  external repo cache, `/private/var/tmp/_bazel_<user>/<hash>/external`) and their files join the
  code map (symbol search, references, callers, outlines) and document search. External files are
  keyed by their **absolute** path — repo files stay repo-relative, so the two namespaces never
  collide and `find_references` resolves across the boundary. Missing/unreadable roots are skipped
  with a warning; a root inside the repo is ignored. Symlinks are followed (Bazel `external/` is
  symlink-heavy). Extra roots are (re-)indexed on a full `basemind scan` only — the live watcher
  does not track them — and git blame short-circuits with a clear error for external (untracked)
  files. The feature itself changes no index/blob format; a re-scan populates external files (and
  this release's `RELEASE_MINOR` bump wipes + rebuilds `.basemind/` anyway).

### Fixed

- **Git author/keyword lookups routed to the wrong tool.** `search_git_history` and `recent_changes`
  descriptions now state their contracts sharply — author/"what did X do"/keyword questions belong
  to the full-depth `search_git_history` (scans every commit reachable from HEAD), not the bounded
  100-commit `recent_changes` window. Precision is verified against real `git` at full branch depth
  by the new `tests/git_parity.rs` harness (opt-in via `BASEMIND_GIT_PARITY_REPO`).
- **Double-run lock UX.** `basemind scan` / `rescan` now pre-detect a live `basemind serve`/`watch`
  holding the store lock (via a non-blocking `probe_writer_lock`) and print an actionable notice
  naming the holder, exiting cleanly instead of colliding with a raw lock error.
- **Misleading `status` latency.** `status` uses a non-blocking read, so a multi-minute rebuild
  holding the write lock is no longer recorded as the call's wall-clock; it returns immediately with
  `rebuild_in_progress: true` when a writer is live.
- **`cache_stats` no longer hard-fails on a stale/unreadable index.** It reports sizes regardless and
  flags `blob_accounting_ok: false` when orphan accounting had to be skipped (e.g. a schema-version
  mismatch), instead of erroring out.
- **`/bm-stats` works offline.** The plugin command is now CLI-first (like `/bm-doctor`), reading
  `basemind cache stats` / `basemind telemetry` when the MCP server is disconnected.

[#34]: https://github.com/Goldziher/basemind/issues/34

## [0.15.1] — 2026-07-02

Patch release: docs / skills / plugin-manifest only. No schema or `RELEASE_MINOR` change, so
existing `.basemind/` caches are untouched on upgrade.

### Added

- **Dedicated per-capability skills.** Split the "use basemind" guidance into three new
  narrowly-triggerable skills alongside the existing umbrella `basemind` and `basemind-comms`:
  `basemind-code-search` (symbol search / outline / references / callers / call graph /
  implementations / `workspace_grep`), `basemind-git-history` (history / blame / diffs / churn /
  `search_git_history`), and `basemind-documents` (semantic + full-text document search, NER /
  keyword filters, and web ingestion). Mirrored into the Codex / Cursor / OpenCode plugin trees by
  `scripts/sync-plugin-skills.sh`.
- **`llms.txt`.** Root-level [llmstxt.org](https://llmstxt.org) index pointing agents at the
  capability skills, docs, and config schema.

## [0.15.0] — 2026-07-02

Minor release: `RELEASE_MINOR` bumps 14 → 15, so every `.basemind/` index + blob store (including
`git-history.fjall/`) is wiped and rebuilt on the next `basemind scan` — which is also how existing
repos populate the new git-history full-text search index.

### Added

- **`search_git_history` — full-text search over commit history.** Search commit **author name +
  email** and **message summary + full body**, tokenized (lowercased, split on non-alphanumeric)
  and matched as an AND. `field` scopes to `author`, `message`, or `all` (default); `limit` +
  `cursor` paginate like the other git-log tools. Backed by two new git-history partitions — a
  `gh_term_to_ords` inverted term→commit-ordinal index (reusing the newest-first delta-varint
  posting encoding) and a `gh_commit_text_by_ord` body store kept out of the head-decode hot path.
  When the git-history index isn't fresh (read-only session / mid-build) the tool degrades to a
  bounded live walk over the recent window, flagged `partial` (author + summary only). To support
  it, commit extraction now captures `author_email` + full `body`, and the stored commit-meta head
  gains an `author_email` field (`sha ‖ time ‖ author ‖ email ‖ summary ‖ files`).

### Fixed

- **Multi-session MCP contention: the writer→read-only downgrade race.** A `basemind serve` that
  rightfully held the `.basemind/.lock` write lock could still come up read-only when a concurrent
  reader transiently held Fjall's single-holder index lock — leaving the repo with _zero_ writers
  (no auto-scan / watcher / rescan → a silently stale index). Fixed on both single-holder Fjall
  stores: the writer now **retries** a transient `Locked` on open (it already owns `.basemind/.lock`,
  so the contention always clears), read-only openers **probe `.basemind/.lock` and skip the Fjall
  open** entirely when a writer is live (serving from the concurrently-readable blobs), and the
  schema-version check no longer does a throwaway **double-open** that widened the race window. The
  downgrade log now names the real lock holder instead of always blaming "another serve".

### Changed

- **git-history / index / comms Fjall opens** collapsed to a single `Database` open + inline schema
  read (was: a throwaway peek-open before the real open). `GIT_HISTORY_SCHEMA` bumps `+4 → +5` for
  the FTS commit-meta layout, wiping and rebuilding `.basemind/git-history.fjall/` on the next scan
  — a no-op for released users (the git-history index only became user-visible with this release).

## [0.14.0] — 2026-06-30

Minor release: `RELEASE_MINOR` bumps 13 → 14 (a new `SymbolKind::Heading` variant), so every
`.basemind/` index + blob store is wiped and rebuilt on the next `basemind scan` — which is also how
existing repos pick up Markdown headings.

### Added — Markdown / Obsidian support

Markdown files (already parsed, but previously yielding no symbols) are now a first-class surface:
point basemind at an Obsidian vault or any Markdown notes directory and the code-map tools work over
it the same way they do over source. All of this is on by default — `.md` is scanned like any other
file, headings ship via a `src/queries/markdown.scm` override, and the reference graph is harvested
by a fence-aware byte-scan in `extract/l2.rs` (the tree-sitter block grammar models none of these
inline constructs).

- **Headings → outline.** ATX (`#`…`######`) and setext headings become `Heading` symbols, so
  `outline` and `search_symbols` (optionally `kind: "heading"`) navigate a note's structure.
- **Wikilinks → backlinks.** `[[Note]]`, `[[Note#Heading|alias]]`, and `![[Embed]]` become
  references, so `find_references "Note"` returns the notes linking to it. Targets resolve by note
  name (`#anchor` and `|alias` stripped), matching Obsidian.
- **Standard Markdown links → backlinks.** `[text](Note.md)` / `![alt](img/Diagram.png)` also become
  references, normalized to the same note name (directory, `.md`, `#anchor` stripped, `%20`
  decoded), so wikilink and Markdown-link vaults share one backlink graph. External URLs
  (`http(s)://`, `mailto:`) and bare `#anchor` links are ignored.
- **Tags → graph.** Inline `#tag` / nested `#area/sub` and YAML frontmatter `tags:` (both `[a, b]`
  and `- a` list forms) become references keyed on the leading `#`, so `find_references "#project"`
  lists every note carrying that tag. Tags inside fenced code blocks are skipped.

Not yet mapped (deliberate follow-ups): block references (`^id`, `[[Note#^id]]`), frontmatter
aliases / arbitrary properties, and task/callout items.

## [0.13.0] — 2026-06-30

Minor release: `RELEASE_MINOR` bumps 12 → 13, so every `.basemind/` index + blob store is
wiped and rebuilt on the next `basemind scan` (intentional; no action needed).

### Fixed

- **`serve` watcher no longer pegs multi-core CPU on gitignored / nested-`.basemind` churn
  (#33).** A writer `serve` watching an umbrella repo woke on every filesystem event except those
  under its _own_ `.basemind/`, then ran a no-op incremental scan that still re-serialized the
  index (`store.flush`) and rebuilt the **entire** `MapCache` over the whole corpus — on every
  debounced batch. With nested child repos each flushing their own `.basemind/` in a mutual loop
  (or any `node_modules` / build churn) this rebuilt the cache indefinitely. Three fixes:
  - The watcher now drops every event a full scan would never index — including **nested**
    child-repo `.basemind/` writes — before waking a rescan, honoring the **full nested
    `.gitignore` hierarchy** (via the `ignore` crate) plus the configured exclude globs. The
    incremental `scan_paths` is now gitignore-aware too, matching full-scan semantics.
  - `scan_paths` short-circuits when a batch changes nothing indexable — no `store.flush`, no
    work.
  - `scan_and_refresh` updates the `MapCache` **incrementally** for the changed paths (re-reading
    only those blobs) instead of rebuilding the whole corpus, and skips the rebuild and
    cache-generation bump entirely when nothing changed (which no longer needlessly invalidates
    paginating clients' cursors).
  - Note: on macOS, FSEvents is kernel-recursive, so excluded subtrees cannot be un-watched;
    their events are now filtered in-process in microseconds instead of triggering a rebuild.

### Changed

- Extracted the scanner's path-filtering (`Filters`, the new `IndexFilter`) into
  `src/scanner_filter.rs` to keep `scanner.rs` under the 1000-line cap.

## [0.12.2] — 2026-06-29

Patch release: blob and index formats are unchanged (`RELEASE_MINOR` stays 12), so no
`.basemind/` rebuild.

### Changed

- **`tree-sitter-language-pack` `1.9.0-rc.45` → `1.12.0`.** The grammar bootstrap
  (`ensure_grammars`) now uses tslp's `prefetch`, which probes real on-disk loadability
  instead of the in-memory `has_language` registry — replacing basemind's hand-rolled
  `DownloadManager::ensure_languages` workaround for the pre-1.12 `download()` short-circuit.
  tslp 1.12's lock-free static `get_language` fast path also removes a global-mutex hop on the
  per-thread parser pool's hot path (tree-sitter is ~30% of scan time). Validated against the
  full harden harness: all 8 OSS repos pass with 0 extraction failures and scan times at or
  under baseline.

Patch release: blob and index formats are unchanged (`RELEASE_MINOR` stays 12), so no
`.basemind/` rebuild.

### Fixed

- **Multiple concurrent sessions on one repo no longer break code-map navigation.** fjall takes
  an exclusive directory lock, so only one `basemind serve` process can open a repo's index at a
  time; additional editor/agent sessions fall back to read-only and previously got an error
  (`read_only_index_unavailable`) from every fjall-backed tool — the reported "blocked / not
  responsive". `find_references`, `find_callers`, `find_implementations`, and `call_graph` now
  answer from in-RAM indexes built from the content-addressed blobs (which are concurrently
  readable), so unlimited sessions work against one repo: one writer on fjall, N readers on the
  shared blobs. `outline` / `search_symbols` / `dependents` / git tools already worked read-only.

## [0.12.0] — 2026-06-29

Minor release: the document/RAG and web-crawl engines move to their renamed,
MIT-relicensed crates. `RELEASE_MINOR` bumps to 12, so the blob + index formats
are considered incompatible — `.basemind/` is wiped and rebuilt from source on
the next `basemind scan`.

### Changed

- **Document tier now builds on `xberg` `1.0.0-rc.1`** (was `kreuzberg` `5.0.0-rc.35`)
  and the web-crawl tier on `crawlberg` `1.0.1` (was `kreuzcrawl` `0.3.0-rc.55`). These
  are the renamed, **MIT-licensed** successors of the same engines, so the `documents`,
  `memory`, and `crawl` feature surfaces are unchanged. The `Elastic-2.0` license
  exceptions for the old crates are dropped from `deny.toml`.
- **Document extraction migrated to xberg's async `extract` API.** xberg 1.0 removed the
  synchronous `extract_file_sync` wrapper; the scanner's doc path now drives the async
  `extract(ExtractInput, &ExtractionConfig)` on a shared multi-thread runtime and reads the
  per-document result out of the new batch-shaped `ExtractionResult.results`.
- **`arc-swap` `1.9.1` → `1.9.2`** via `cargo upgrade`. `arrow-array` / `arrow-schema`
  stay pinned to `58` to match lancedb 0.30's transitive arrow.
- **Web crawling now blocks private/loopback/link-local targets by default** (SSRF
  protection inherited from crawlberg 1.0). `web_scrape` / `web_crawl` / `web_map` against
  `127.0.0.1`, `10.0.0.0/8`, `169.254.0.0/16`, etc. are rejected with an SSRF-policy
  violation. A new `crawl.allow_private_network` config flag (default `false`) re-enables
  them for internal-docs hosts you control; the `CRAWLBERG_ALLOW_PRIVATE_NETWORK` env var is
  also honoured as a process-wide override.

### Fixed

- **Status line no longer renders blank on Linux** — `build_basemind_line` read file mtimes
  with BSD `stat -f %m` first. On GNU coreutils `-f` means "display filesystem status" and
  _succeeds_, printing a multi-line blob instead of failing, so the `|| stat -c %Y` fallback
  never ran; under `set -euo pipefail` the blob's bare `File` word aborted the command
  substitution as an unbound variable, emptying the whole line. An `epoch_mtime` helper now
  tries GNU `stat -c %Y` first and falls back to BSD `stat -f %m` only when `-c` genuinely
  fails (macOS), used at both mtime call sites (#32).

Patch release: blob and index formats are unchanged (`RELEASE_MINOR` stays 11), so no
`.basemind/` rebuild. A `tar` security bump on the npm installer, a Homebrew-tap fix, and
raised runtime floors.

### Security

- **npm installer bumps `tar` 6 → 7.5.16** (GHSA-vmf3-w455-68vh) — the bundled extractor was on a
  `tar` line whose PAX size-override parsing differential allows file smuggling. The fix only exists
  in `tar` 7.5.16+, which is ESM-only and requires Node ≥ 18; `install.js` now pulls `tar` in via a
  dynamic `import()` at the extract site, and the npm package's engine floor moves to Node ≥ 22.

### Fixed

- **Homebrew tap no longer breaks on Intel hosts** — the generated formula's `on_intel` block called
  `odie` at formula-load time, which aborted every `brew` command that read the tap on an Intel Mac
  (poisoning bottle builds and installs for _all_ formulae in the tap, not just basemind). The
  Apple-Silicon-only constraint is now expressed via `depends_on arch: :arm64`, evaluated at install
  time instead of load time.

### Changed

- **Runtime floors raised** — the npm package (Node ≥ 22, was ≥ 14) and the opencode plugin
  (Node ≥ 22, was ≥ 18); pip package requires Python ≥ 3.10 (was ≥ 3.8). Older runtimes are
  end-of-life.

## [0.11.0] — 2026-06-26

Minor release: `RELEASE_MINOR` bumps to 11, so the blob and Fjall index formats are considered
stale and every `.basemind/` is wiped and rebuilt from source on the next `basemind scan` (one-time,
intentional). The headline is a precomputed git-history index that turns the history MCP tools from
full-walk-per-query into sub-millisecond lookups, plus an install fix that unblocks Apple Silicon
Macs running an x86_64 shell, Node, or Python under Rosetta.

### Added

- **Precomputed git-history Fjall index** — `commits_touching`, `find_commits_by_path`, and
  `symbol_history` previously walked the commit graph on every call, paying an O(history) cost per
  query. They are now backed by a per-repo Fjall index built once at scan time and refreshed
  incrementally for new commits, so queries are O(result) sub-millisecond lookups. Posting lists are
  stored newest-first, keeping `commits_touching` O(n) in the returned commits rather than in total
  history depth.

### Fixed

- **Apple Silicon installs under Rosetta no longer abort** — `uname -m`, `os.arch()`, and
  `platform.machine()` all report the _process_ architecture, so an x86_64 shell, Node, or Python
  running under Rosetta reports `x86_64` on Apple Silicon hardware. All three installers (the shell
  launcher, npm `install.js`, pip `downloader.py`) matched the Darwin/x86_64 branch and aborted with
  "Intel macOS not supported", even though the native arm64 binary runs fine. They now probe a
  hardware-level signal the translation layer cannot spoof (`sysctl -n hw.optional.arm64`) and
  resolve `aarch64-apple-darwin` whenever that probe — or the reported arch — indicates arm64.
  Fixes #28.
- **Windows `harden` test compiles again** — the new on-disk index-size measurement used
  `std::os::unix::fs::MetadataExt::blocks()` unconditionally, breaking the `windows-latest` test
  leg's compile. The block-count path is now `cfg(unix)`-gated with a `len()` fallback on other
  platforms.

### Performance

- **Path-scoped history walks** — `commit_touches_path` computed the full recursive tree diff of
  every walked commit just to test membership of one path, so a single history query over a deep
  monorepo could run for minutes (measured >5 min on a 242k-commit repo). It now looks up the entry
  at the exact path in the commit tree and each parent via `gix` `lookup_entry` (O(path depth) object
  reads, no sibling recursion), comparing `(blob oid, mode)`. Semantics are identical to
  `git log --full-history -- <path>`; the same query drops to ~11 s worst case and ~2.5 s typical.

### Changed

- **Dependencies** — bumped the embedded `rmux` shells trio (`rmux-client` / `rmux-sdk` /
  `rmux-server`) from 0.6 to 0.7 and refreshed the lockfile for compatible updates. `arrow` stays
  pinned at 58 to match what `lancedb` resolves transitively.

## [0.10.3] — 2026-06-25

Patch release: blob and index formats are unchanged (`RELEASE_MINOR` stays 10), so no
`.basemind/` rebuild. Three Windows-only correctness fixes surfaced once the comms suite and the
full tool sweep run on the `windows-latest` CI leg, plus install/release hardening so a partial
publish can no longer leave the plugin launcher unable to install.

### Fixed

- **Windows full scan now produces `/`-separated index keys** — the full-scan walker was optimized
  to feed `Path::to_str()` straight into the index without the `\`→`/` normalization the incremental
  `scan_paths` path still did, so on Windows every nested file was keyed with backslashes
  (`vendored\inner.rs`) and forward-slash lookups missed — breaking all subdirectory queries
  (outline / search / references for any file not at the repo root). Normalize at the walker source;
  the extra allocation is Windows-only and never touches the Unix hot path.
- **Windows comms named pipe now isolates by `comms_dir`** — `comms_socket_path` derived the
  Windows pipe name from the username only (`\\.\pipe\basemind-comms-<user>`), ignoring
  `BASEMIND_COMMS_DIR`, so every comms dir on a host collapsed onto one per-user singleton pipe.
  Parallel comms suites (each isolated to its own tempdir) collided — daemons cross-contaminated,
  one test's teardown killed another's daemon, and concurrent `comms start` races hung to the CI
  timeout. The pipe name now mixes in a hash of `comms_dir`, mirroring the per-dir Unix socket;
  production (which leaves `BASEMIND_COMMS_DIR` unset) keeps a single stable per-user broker. A
  `timeout-minutes: 30` backstop on the CI test job prevents any future hang from blocking the
  queue-not-cancel main concurrency.
- **Windows `comms start` no longer hangs** — the detached broker daemon inherited the launcher's
  stdout/stderr: on Windows `CreateProcess` runs with `bInheritHandles = TRUE` whenever stdio is
  redirected, leaking every inheritable handle into the child — including the pipe a parent captured
  via `Command::output()` (or `serve`'s MCP stdio). The long-lived daemon then held the write end
  open, so the capturing parent never saw EOF and blocked until the daemon died. Unix is immune
  (`Stdio::null` dup2's `/dev/null` over the child fds and Rust sets `CLOEXEC` on its own pipes).
  `spawn_detached_daemon` now clears `HANDLE_FLAG_INHERIT` on its std handles before the detached
  spawn, so the daemon inherits none of them; the real-daemon comms E2E suite runs on Windows again.
- **Plugin launcher names an incomplete release instead of failing opaquely** — when a pinned
  release is missing a platform binary or its checksums file (a partial publish, as 0.10.0 was),
  `mcp-launch.sh` died with a bare "download failed" / "could not fetch checksums" that the MCP
  client surfaced only as "failed to connect". It now reports the release as incomplete and tells
  the user to update the plugin (Claude Code: `/plugin update`) to a complete release.
- **Releases publish atomically** — `create_release` made the GitHub release live before its
  binaries finished uploading, so a failed platform build (as 0.10.0's Linux legs) left it
  half-populated with no checksums, and the launcher's checksum-verified download then fails closed
  for every user pinned to it. The release is now created as a **draft**; a `finalize_release` job
  promotes it only once all four platform archives **and** the checksums file are present (and the
  npm / PyPI / Homebrew publishes gate on that finalize, since they download from the release). A
  failed platform build now leaves a hidden draft, never a live, broken release.

## [0.10.2] — 2026-06-25

Patch release: blob and index formats are unchanged (`RELEASE_MINOR` stays 10), so no
`.basemind/` rebuild. Fixes the Linux release archive, which 0.10.1 shipped broken on a clean host.

### Fixed

- **Linux release binaries: bundled libraries could not find their siblings** — the archive
  bundles native `.so`s (libheif, libaom, …) into `lib/`, but only the main binary carried the
  `$ORIGIN/lib` rpath. A bundled lib with a sibling dependency (`libheif.so.1` → `libaom.so.3`,
  both in `lib/`) had no rpath of its own, so on a clean host the loader failed with
  `libaom.so.3: cannot open shared object file` — even on `basemind --version`. The in-container
  packaging smoke missed it because the build container has those codecs system-installed.
  `package-release.sh` now sets `$ORIGIN` on every bundled lib so sibling-to-sibling deps resolve,
  verified by running the real release artifact on a clean glibc-2.28 host.

## [0.10.1] — 2026-06-25

Patch release: blob and index formats are unchanged (`RELEASE_MINOR` stays 10), so no
`.basemind/` rebuild. This release completes the 0.10.0 distribution — it ships the Linux
binaries and the npm / PyPI / Homebrew packages that 0.10.0 could not produce.

### Fixed

- **Linux release binaries (x86_64 + aarch64)** — the 0.10.0 Linux build linked ort's prebuilt
  ONNX Runtime with `cargo-zigbuild`, which fails because zig links LLVM `libc++` while the
  prebuilt is built against GNU `libstdc++`. The Linux binaries now build inside the official
  `manylinux_2_28` containers with native gcc. The prebuilt ort also references glibc 2.38 symbols
  (`__isoc23_strtol/ll/ull`, `__libc_single_threaded`); a small compatibility shim
  (`scripts/glibc228_compat.c`) backfills them so the binaries keep a **GLIBC_2.28** floor
  (RHEL 8 / Debian 11 / Ubuntu 20.04+ / Amazon Linux 2023). Because 0.10.0's binary legs failed,
  its checksums and the npm / PyPI / Homebrew publishes were skipped; 0.10.1 carries the same
  source and completes every distribution channel.

## [0.10.0] — 2026-06-24

Minor release: `RELEASE_MINOR` bumps 9 → 10, so the on-disk index version changes and every
`.basemind/` cache rebuilds from source on the next `scan`. This rebuild is **one-time and
harmless** — the blob and index formats are unchanged.

### Added

- **Agent shells: embedded rmux daemon + six MCP tools** — spawn detached headless shell sessions
  via `shell_spawn` with optional cwd, env overrides, and title. Drive sessions with `shell_send`
  (write stdin), `shell_capture` (visible screen), `shell_broadcast` (multicast input), `shell_list`
  (enumerate with liveness), and `shell_kill` (terminate). basemind embeds the daemon (re-execs
  itself with `--__internal-daemon` — no external `rmux` binary). Sessions are long-lived across
  tool calls and driven headless via MCP.
- **Visual attach** (Unix + Windows) — when `[shells].visual` is not `headless`, `shell_spawn` opens
  the session in a terminal tab/window attached to it via a hidden `basemind --__internal-attach`
  re-exec (macOS / Linux emulators or Windows Terminal `wt.exe`; basemind ships no external `rmux`
  binary). Presentation is best-effort — a spawn never fails just because no terminal could be driven
  — and the response's `attach_command` is returned for manual re-attach.
- **Comms-coupled session rooms** (Unix + Windows, `comms` feature) — spawned children auto-join a
  session-scoped comms room via inherited `BASEMIND_SESSION_ID` / `BASEMIND_PARENT_AGENT_ID` /
  `BASEMIND_AGENT_ID`, enabling bidirectional parent↔child messaging and forming parent→child
  inheritance chains across agents.
- **`[shells]` configuration sub-tree** — controls visual presentation mode (`current` / `window` /
  `web` / `headless`; default `current` = new tab in open terminal), terminal emulator choice
  (`auto` / `iterm2` / `terminal_app` / `windows_terminal` / `gnome_terminal` / `konsole` /
  `wezterm` / `alacritty` / `kitty` / `xterm`), and session pty dimensions (`default_cols` /
  `default_rows`) and lifecycle (`keep_on_exit`). Requires `--features shells`.
- **Multi-agent comms orchestration** — one `serve` now drives many named sub-identities over a
  single connection via a per-call `as_agent` parameter, backed by a per-identity broker-client
  registry parented to the orchestrator. New `dm_send` delivers a direct message into another
  agent's inbox over a private pairwise room; `agent_register` / `agent_list` advertise and discover
  the live roster. The whole surface is mirrored on the CLI (`basemind comms … --as-agent`, `dm`,
  `room-for-path`), and a `multi-agent-room` skill + a code-review-panel demo show the pattern.
- **Per-repo comms rooms + recency** — agents auto-join a default room keyed by the repo, created on
  demand via `get_or_create_chat_room_for_path`; `Global` is repurposed for machine-wide ops
  coordination. Reads are recency-aware: `room_history` / `inbox_read` default to the last 24h
  (`since_hours` widens, `0` reads the full append-only log), each message carries `age_secs`, and
  `room_list` flags a room `stale` after 7 days of silence.
- **Windows support for agent comms + shells** — the comms broker (named-pipe transport, singleton,
  signal handling) and the embedded rmux shell runtime now build and run on Windows, and the comms
  MCP tools are exposed there. `--features full` therefore ships agent comms and agent shells on
  every published platform — macOS, Linux, and Windows — not Unix only. A dedicated Windows CI job
  builds and tests the `comms` + `shells` surface.

### Changed

- **Dependency refresh** — kreuzberg `5.0.0-rc.30` → `rc.35` (document-tier extraction stack) and
  `rmcp` `1.7` → `1.8`. `arrow` stays at 58 (lock-step with lancedb 0.30). rmcp 1.8 deprecates MCP
  logging (SEP-2577); basemind keeps the capability for now since the status line and `rescan`
  progress depend on it.
- **Linux release binaries target glibc 2.28** (RHEL 8 / Debian 11 / Ubuntu 20.04+ / Amazon Linux
  2023) — the two `*-unknown-linux-gnu` artifacts are built with `cargo-zigbuild` (zig as the
  linker), pinning the required glibc symbol floor without changing runtime behaviour (still
  dynamically linked). A CI `objdump` guard fails the build if a dependency pulls a newer symbol.
  macOS / Windows artifacts are unchanged.

### Removed

- **Experimental A2A server (`--features a2a`)** — the gRPC + JSON-RPC/SSE Agent-to-Agent server,
  its protobuf codegen + `proto/`, the `basemind a2a serve` command, the buf CI job, and the
  a2a-only dependency stack (tonic, prost, axum, axum-server, tower, tower-http, rustls, reqwest,
  subtle, rcgen, …) are removed. It was opt-in and never part of the shipped release surface; the
  maintenance cost (pinned codegen, held-back deps, a dedicated buf CI job) outweighed its
  experimental value. The comms agent-card shapes stay A2A-schema-aligned, so an HTTP front-end can
  still be added behind the `CommsFrontend` trait later without it.

### Fixed

- **Comms daemons no longer pile up** — a busy broker that missed a single liveness ping could be
  wrongly judged dead and have its socket unlinked-and-rebound by a new daemon, orphaning the
  original on a dangling socket that nothing reaped promptly; across many sessions dozens
  accumulated. Two guards: the liveness probe now retries before declaring a daemon dead (so a
  live-but-busy broker is not reclaimed), and a Unix socket-ownership watchdog makes any daemon
  whose socket was unlinked or replaced self-terminate within seconds instead of lingering as an
  orphan.
- **Comms daemon takes over a previous build on load** — when a serve or CLI brings up the broker
  and finds the socket held by an older or protocol-incompatible daemon, it now asks that daemon to
  stop and spawns a current one in its place, converging the singleton on the newest binary; if the
  predecessor will not yield, it errors out clearly (naming the version + pid) instead of silently
  talking to an incompatible daemon — which is how the pre-0.10 skew surfaced as an opaque
  "connection closed". A same-version (or newer) daemon is reused, so concurrent sessions still
  share one broker.
- **Comms messages now expire** — the broker prunes messages past a 7-day TTL on startup and hourly
  thereafter, so the user-global comms store cannot grow without bound. Recency-aware reads already
  hid old messages; this reclaims their storage. Room records and per-room sequence counters are
  left intact so read cursors stay monotonic.
- **Concurrent serve sessions no longer collide** (#26, #27) — the editor plugin spawns one
  `basemind serve` per session, but the store write lock is single-holder. A contending serve now
  starts in a **read-only** mode (instead of exiting and handing the MCP client an opaque `-32000`),
  so its tools register and the session is usable: `outline`, `search_symbols`, `list_files`,
  `dependents`, `workspace_grep`, and the git tools answer from the in-RAM map and git. Fjall takes
  an exclusive lock on its index, so the call/reference tools (`find_references`, `find_callers`,
  `find_implementations`, `call_graph`) cannot read it from a second process — they now return a
  clear "held by another basemind serve" error rather than a misleading empty result. The
  lock-holding serve stays the sole writer; a read-only serve's `rescan` returns the same clear
  error. Lock contention fails fast with the clean lock message rather than the multi-GB busy-spin
  reported on 0.9.0 (#26).
- **Agent-shells hardening** — `shell_spawn` now honours the `[shells].enabled` master switch,
  confines `cwd` to the repository root (rejecting `..` / absolute escapes), threads the configured
  `default_cols` / `default_rows` into the spawned pty, validates `BASEMIND_SHELLS_SOCKET` on the
  client path, rejects carriage returns in env keys/values, and widens the loader-injection warning
  list (`LD_AUDIT`, `DYLD_FALLBACK_LIBRARY_PATH`). The shells and comms modules build cleanly on
  both Unix and Windows (see Added); a build without those features still excludes them entirely.
- **Status line recognizes pre-0.9 indexes** — the basemind status line keyed its "scanned yet?"
  check on the fused `.fm` blob introduced in 0.9, so an index written by an earlier binary (split
  `.l1`/`.l2` blobs) showed "scanning…" indefinitely though it was healthy. It now accepts both
  layouts and never double-counts the secondary blob layer.

### Security

- **`quinn-proto` bumped to 0.11.15** (RUSTSEC-2026-0185) — fixes a remote memory-exhaustion vector
  via unbounded out-of-order QUIC stream reassembly, pulled transitively under `--features full`.
- Known/tracked advisories carried this release (no fix available yet): `bincode` 1.3.3 unmaintained
  (via `rmux-proto`), `memmap2` 0.9.10 unsound pointer offset (via `lancedb`, documents/memory).

## [0.9.0] — 2026-06-23

Minor release: `RELEASE_MINOR` bumps 8 → 9, so the on-disk index version changes and every
`.basemind/` cache rebuilds from source on the next `scan`. This rebuild is **structural** — the
per-file extraction blob format changed from two files (`<hash>.l1.msgpack` + `<hash>.l2.msgpack`)
to one combined frame (`<hash>.fm.msgpack`); any old split blobs left on disk are reclaimed by
`cache gc`. A scanner hot-path pass cuts cold-scan wall time ~22%. Bumps kreuzberg to `5.0.0-rc.30`.

### Changed

- **Scanner: ~22% faster cold scans** — two flamegraph-ranked wins. (1) Per-file Fjall index commits
  are now batched per rayon worker (256 files per commit), removing the ~14% of scan time worker
  threads spent serializing on Fjall's single write lock. (2) The L1 outline and L2 calls are fused
  into one content-addressed blob (`<hash>.fm.msgpack`), so the default eager-L2 scan does one
  `open` + atomic `rename` per file instead of two. Measured min-of-7 cold scans on a 6.7k-file repo:
  2.68 s → 2.07 s; the per-file read-before-write index atomicity is preserved.
- Bump **kreuzberg to `5.0.0-rc.30`** — picks up the upstream Tesseract `tessdata`-path fix, so the
  released binary resolves trained data at runtime instead of baking in the build runner's path.
  Resolves #12.
- Dependency refresh to latest compatible (gix 0.85, criterion 0.8, …). arrow stays at 58 to match
  lancedb 0.30; the experimental `a2a` feature's gRPC deps are held pending a tonic-0.14 migration.

### Fixed

- **Comms daemons no longer leak across sessions.** A daemon orphaned by a dead session (reparented
  to pid 1) used to keep its socket + flock and never exit, so they accumulated indefinitely. The
  broker now tracks connected links + last activity and self-terminates after 30 min with no clients,
  via the normal drain path. A live client (even a quiet subscriber) keeps it alive. Unix-only.
- Removed the document-tier troubleshooting workarounds from the README now that both upstream
  kreuzberg bugs are fixed (TLS-MITM in rc.29, `tessdata` in rc.30). Resolves #14.

### Removed

- The pre-0.9 split-tier blob format (`.l1.msgpack` / `.l2.msgpack`), superseded by the combined
  `.fm.msgpack` frame. `cache gc` reclaims any left behind by the schema-bump refresh.

## [0.8.0] — 2026-06-22

Minor release: `RELEASE_MINOR` bumps 7 → 8, so the on-disk index version changes and every
`.basemind/` cache rebuilds from source on the next `scan` (one-time; the blob/index formats are
unchanged, so the rebuild is harmless). Deepens the MCP surface — prompts, argument completions,
logging, and progress notifications — and hardens the consumer experience: cleaner document-tier
scans, a cold-start scan path agents can invoke, serve diagnostics, and a recovery runbook. Bumps
kreuzberg to `5.0.0-rc.29`.

### Added

- **MCP prompts** (`prompts/list` + `prompts/get`) — four reusable, parameterized workflows that
  teach a client to drive basemind structure-first: `onboard-repo`, `trace-symbol`, `explain-file`,
  `review-working-tree`.
- **MCP argument completion** (`completion/complete`) — autocompletes prompt arguments from the
  in-RAM code map: `trace-symbol`'s `symbol` against indexed symbol names, `explain-file`'s `path`
  against indexed file paths (pure prefix scan, no store lock).
- **MCP logging + progress** — `logging/setLevel` plus a `rescan_complete` log with scan counts,
  and start/done progress notifications on `rescan` when the client supplies a progress token.
- **Tool annotations** on every MCP tool (`read_only_hint` / `destructive_hint` / `idempotent_hint`
  / `open_world_hint`) so clients (Claude Code et al.) can auto-approve read-only tools and only
  prompt for mutating ones.
- **Cold-start indexing for agents** — a `bm-scan` command + `basemind-scan` skill that run
  `basemind scan` via the CLI (no MCP server required), so an agent can build the index when
  basemind reports "no index".
- **Recovery runbook** — a `bm-doctor` command + `basemind-doctor` skill: diagnose the index, detect
  a stale lock via the `.lock.meta` holder pid, rebuild via scan, and reconnect the MCP server
  (client-specific). Plus `basemind serve` now logs its lifecycle (startup pid/version/view and the
  exact exit reason) to its stderr / the client's MCP server logs.

### Changed

- Bump **kreuzberg to `5.0.0-rc.29`** — picks up the TLS-certificate fix for model downloads behind
  corporate TLS-MITM proxies (`hf-hub` now uses the platform `native-tls` provider, honoring the OS
  cert store + `SSL_CERT_FILE`/`SSL_CERT_DIR`). Resolves #14.

### Fixed

- The document tier no longer counts non-extractable files as failures. A file tree-sitter doesn't
  recognize as code (e.g. an Erlang `.app.src`, which `mime_guess` maps to
  `application/x-wais-source`) is now **skipped** — and logged at debug — instead of inflating the
  failed count; genuine extraction errors on real documents still count as failures.

## [0.7.0] — 2026-06-22

Minor release: `RELEASE_MINOR` bumps 6 → 7, so the on-disk index version changes and every
`.basemind/` cache rebuilds from source on the next `scan` (intentional, one-time). The headline is a
first-class **code-aware token-reduction** surface (the `compress`/`expand` tools, per-call budgets,
TOON encoding, behavioral output compression) plus **code-grounded memory/skill governance**
(`memory_audit` + git-mined skill proposals). Also ships MCP tool annotations so clients can
auto-approve read-only tools, and a sweep of CLI/MCP bug fixes.

### Added

- **`compress` MCP tool** — code-aware compression (tree-sitter structural elision + cheap lexical
  passes) with an honest before/after token report; **`expand`** pulls a single symbol's body back.
- **Per-call `max_tokens` budgets** on high-volume tools (`outline`, `search_*`, `find_references`,
  `workspace_grep`, `search_documents`, …): rank-to-fit with an explicit truncation marker + cursor.
- **Opt-in TOON encoding** (`format:"toon"`) for high-volume list responses, and an opt-in **lean
  tool surface** (`BASEMIND_MCP_LEAN`) that advertises three wrapper tools instead of the full set.
- **Behavioral output compression** (`basemind compress-output`/`delta`/`checkpoint`/`detect-waste`)
  with credential-safe, fail-open semantics, plus opt-in plugin hooks (Bash-output compression,
  read-cache deltas).
- **Code-grounded governance** — `memory_audit` verifies stored memories against the live code map
  (structural-hash drift → stale, decay + 90-day auto-archive) and git-mined **skill proposals**
  (`proposals_mine`/`proposals_list`/`proposal_accept`/`proposal_reject`); additive
  provenance/verification fields on `MemoryRecord` and two new lazy Fjall keyspaces.
- **MCP tool annotations** on all tools (`read_only_hint`/`destructive_hint`/`idempotent_hint`/
  `open_world_hint`) so consumers (Claude Code et al.) can auto-approve read-only tools.
- **Real tokenizer-backed token counts** (o200k under `documents`) in the compress report and
  telemetry, and instructive `-32602` parameter errors via a `Lenient<T>` wrapper + serde aliases
  (accept `query`/`pattern`/`needle`/`name`/`regex`/… across the search tools).
- Plugin integrations for **Kimi Code** and **pi**, and a Codex-readable marketplace manifest.

### Changed

- Bump **kreuzberg to `5.0.0-rc.28`**.
- Honest telemetry: `est_tokens_saved` now credits document/list/web tools instead of reporting 0,
  and `search_symbols.total` (`total_is_partial`) / `list_files.limit_clamped` / `status.blob_count`
  expose what was previously silent.
- `scan` / `rescan` exit `0` when per-file read failures occur but the index was still updated
  (previously exit 2 masked a successful scan); `scan --rev` skips submodule gitlinks instead of
  failing; `-q`/`--quiet` now suppresses subsystem INFO/WARN logs.
- `cache clear` gains a `views:<name>` selector to clear a single view without nuking all of them.
- Slimmer tool descriptions to cut per-session schema tokens.

### Fixed

- Lock-contention errors now name the actual holder (`serve`/`watch`/`scan`/`rescan`) via a `.lock`
  sidecar instead of always guessing `watch` (#11).
- `query` commands resolve absolute and `./`-prefixed paths to the indexed key (#19); serving or
  querying a never-scanned named view now errors instead of silently returning empty (#18).
- `comms status` gives an actionable "daemon not running — start with `basemind comms start`" message
  (#21); comms verbs no longer emit a false "`--json` has no effect" warning (#20).
- `git_cache_bytes` documented as the disk layer only (#23); CLI lock / LanceStore-shutdown crashes
  and legacy comms front-matter decoding fixed.
- Known upstream issues in the document tier (OCR `tessdata` path #12, model download behind a
  TLS-MITM proxy #14) are documented with workarounds; fixes tracked in kreuzberg and picked up on a
  later bump.

## [0.6.3] — 2026-06-21

Patch release: schema unchanged (`RELEASE_MINOR` stays 6). Reworks the plugin MCP launcher so it
no longer depends on `npx`/`uvx` at runtime — fixing intermittent start-up failures when several
agent sessions (or the comms-monitor poll loop) launch basemind concurrently.

### Fixed

- **Plugin launcher npx/uvx race** — `scripts/mcp-launch.sh` previously exec'd `npx basemind@<ver>`
  (then `uvx`) as the runtime. npx stages into a shared, spec-hashed `~/.npm/_npx/<hash>` directory,
  so two concurrent launches raced on it and failed with `ENOENT … package.json`; the binary was also
  never cached, so every launch re-resolved over the network and inherited node/python start-up cost
  plus lavamoat postinstall blocks. The launcher now has a single install method: download the
  checksum-verified prebuilt release binary once into a stable per-user cache
  (`${XDG_CACHE_HOME:-~/.cache}/basemind/bin/<version>/`), serialized with an atomic lock, and exec it
  directly on every subsequent launch. `npx`/`uvx` and the `BASEMIND_LAUNCHER` override are removed;
  set `BASEMIND_BIN=/path/to/basemind` to point at a local dev build. The npm/PyPI/Homebrew/cargo
  install channels are unchanged.

## [0.6.2] — 2026-06-21

Patch release: schema unchanged (`RELEASE_MINOR` stays 6). Fixes the macOS binary so the
npm/PyPI/Homebrew/cargo wrappers actually run — 0.6.0 and 0.6.1 shipped an unrunnable macOS binary.

### Fixed

- **macOS binary SIGKILL on launch** — the release packaging bundles non-system dylibs into `lib/`
  and rewrites their `@loader_path` install names with `install_name_tool`, which edits the Mach-O in
  place and invalidates the linker's ad-hoc code signature. macOS then killed the process on first
  page-in (`SIGKILL`, "Code Signature Invalid") with no output, so the binary downloaded by every
  wrapper was unrunnable. The packaging now re-signs each dylib and the binary ad-hoc **after** the
  rewrites and verifies `--strict` before packaging.
- **pip/uvx downloader** — a stale or partial `~/.cache/basemind/<ver>/` made `os.replace()` fail with
  `Errno 66` (cannot overwrite a non-empty directory), wedging installs until a manual cache wipe. The
  downloader now clears a stale cache dir under its lock and retries the atomic move.

## [0.6.1] — 2026-06-21

Patch release: blob/index/comms schema unchanged (`RELEASE_MINOR` stays 6), so no
`.basemind/` rebuild. Fixes the Windows release build that blocked the 0.6.0 wrapper publish.

### Fixed

- **Windows build** — the agent-comms substrate is built on Unix domain sockets
  (`UnixStream`/`UnixListener`, peer-cred auth) with no Windows analogue, but `comms`
  imported `tokio::net::UnixStream` ungated, so the `--features full` `x86_64-pc-windows-msvc`
  build failed to compile (since comms landed in 0.5.0). This failed the Windows binary job and
  held back the npm/PyPI/Homebrew publishes. The comms surface is now gated on
  `all(feature = "comms", unix)` — on Windows the binary compiles with comms absent; unix is
  unchanged. 0.6.0 published to crates.io only; 0.6.1 is the first complete cross-channel release
  of the 0.6 line.

## [0.6.0] — 2026-06-20

Minor release: `RELEASE_MINOR` bumps 5 → 6, so the blob, Fjall-index, and LanceDB schema versions
advance — the first `basemind scan` / `serve` after upgrading **wipes and rebuilds the `.basemind/`
cache and the LanceDB store in place**. Headline: an **experimental, opt-in Agent-to-Agent (A2A)
task server**, plus scanner/query hot-path perf wins and release-pipeline hardening.

### Added

- **Experimental A2A server** — `basemind a2a serve`, gated behind `--features a2a` and intentionally
  excluded from the default and `full` builds and from the shipped release binary. One axum listener
  serves the A2A task protocol three ways — gRPC `A2AService`, JSON-RPC 2.0, and SSE streaming — with
  the agent card at `/.well-known/agent-card.json`. Hardened surfaces: **bearer-token auth** (constant-
  time compare, non-loopback bind refused without a token), **TLS termination** (`--tls-cert` /
  `--tls-key`, ALPN h2 + http/1.1), and **push-notification webhook delivery** behind a real **SSRF
  guard** (rejects loopback, RFC-1918, link-local, and the cloud-metadata IP; re-checks the resolved
  address and pins it on connect).
- **`rescan` levers** (CLI + MCP) plus robust auto-rescan on an empty working-view index.
- **comms `inbox_ack`** — advances the per-agent read cursor (idempotent, non-destructive; the shared
  append-only room log is untouched) — and **richer message front-matter** (`ts_micros`, `tags`,
  `reply_to`, `seq`, and an optional `scope` path/glob set for relevance filtering).

### Changed

- **Scanner/query perf** — cache the parse-timeout env lookup, pre-classify L1 captures, and defer the
  walk allocation in the scanner inner loop; serve `outline` from the in-RAM cache to cut query-path
  allocations; bounded-concurrent crawl indexing aligned with the kreuzberg parallelism pool.

### Fixed

- **Atomic release publishing + resilient wrappers** — a version-consistency gate across all shipped
  surfaces, an unconditional checksums job asserting every archive, `needs`/`result == success` guards
  on each publish job, and fail-closed retrying npm/pip installers with an atomic cache. Fixes the
  v0.5.0 broken-install regression (missing checksums file → every wrapper install failed).
- **comms daemon** reconnects and respawns on a broken pipe instead of failing the request.

## [0.5.0] — 2026-06-20

Minor release: `RELEASE_MINOR` bumps 4 → 5, so the blob, Fjall-index, and LanceDB schema versions
advance — the first `basemind scan` / `serve` after upgrading **wipes and rebuilds the `.basemind/`
cache and the LanceDB store in place**. Headline: basemind becomes a multi-agent **communication
substrate** (scoped rooms + per-agent inbox) on top of its context layer, with split memory and
cross-harness delivery.

### Added

- **Agent-to-agent communication.** A singleton, user-global **broker daemon** (its own Fjall
  store over a Unix socket, independent of any repo's exclusive index lock) hosts **scope-joined
  rooms** — an agent auto-joins every room covering its git remote, a path prefix, or global.
  Messages are **two-tier**: a front-matter envelope (subject · from · id) that `room_history` /
  `inbox_read` scan cheaply, plus a body fetched on demand by `message_get`. An agent's **own**
  posts are excluded from its inbox. New MCP tools `agent_register`, `agent_list`, `room_create`,
  `room_list`, `room_join`, `room_leave`, `room_post`, `room_history`, `message_get`, `inbox_read`,
  with full `basemind comms …` CLI parity.
- **Split memory.** Memory gains an **individual** (per-agent) tier alongside the existing
  **group** (shared) tier, selected by a `visibility` parameter; agent identity resolves from
  `BASEMIND_AGENT_ID` → config → persisted `.basemind/agent-id` → `anon`.
- **Cross-harness delivery.** The comms mandate and a "prefer basemind over grep/git/manual"
  directive ship to every harness via the MCP server `instructions`, a new **`basemind-comms`
  skill**, and the generated per-harness instruction files. **SessionStart + UserPromptSubmit
  hooks** inject unread front-matter on boot and per turn; a **background monitor (~15 s)** surfaces
  new messages while an agent works or idles (Claude Code).
- **Statusline comms segment.** The basemind statusline now shows the agent's unread message count
  (bright when non-zero) and identity, gated on a running broker and TTL-cached so it stays cheap.

### Changed

- **Schema bump: `RELEASE_MINOR` 4 → 5.** The blob, Fjall-index, and LanceDB `memory`/`documents`
  schema versions advance, so the first `basemind scan` / `serve` after upgrading **wipes and
  rebuilds the `.basemind/` cache and the LanceDB store in place** (the latter now guarded by a
  `memory_schema_ver` in `meta.json` so the memory-table column add never faults at query time).
  Stored memory is rebuildable scratch, not a source of truth — re-`memory_put` anything you want
  to keep, or expect it to be re-derived.

## [0.4.0] — 2026-06-19

Minor release: `RELEASE_MINOR` bumps 3 → 4, so the blob + index schema versions advance.
The first `basemind scan` / `serve` after upgrading rebuilds the cache in place. Prebuilt binaries
now ship `--features full` (96 document formats, OCR, embeddings, reranker, semantic search, web
crawl, shared memory). **Windows asset triple changed:** `x86_64-pc-windows-gnu` → `x86_64-pc-windows-msvc`
(ONNX Runtime ships no MinGW prebuilts); anyone hard-coding the old asset name must update.
**Intel macOS (`x86_64-apple-darwin`) is no longer shipped** — macOS prebuilts are Apple Silicon
(`aarch64-apple-darwin`) only; Intel Mac users build from source (`cargo install basemind --features full`).

### Added

- **Full-feature prebuilt binaries via native per-platform runner.** All prebuilt binaries
  (Homebrew, npm, pip, GitHub Releases) now ship `--features full`: 96 document formats (PDF,
  Office, email, HTML, archives, structured, source, markup), OCR (Tesseract), HEIC/AVIF
  (libheif), embeddings + reranker + NER, shared memory, and web crawl. First use downloads ML
  models over the network; binaries are larger. Replaced goreleaser with a new native
  per-platform CI runner pipeline.
- **Enforced binary checksum verification.** All install paths (npm, pip/uvx, direct download via
  `mcp-launch.sh`) now verify the binary's sha256 against `basemind_<version>_checksums.txt` and
  fail closed on mismatch or missing checksum. (This was previously claimed in the README but not
  implemented.)

### Changed

- **`find_references` and `find_implementations` now do real substring matching.** Previously
  documented as substring matching but actually exact-only. Now genuinely substring (case-sensitive),
  so `Foo::bar()` and `bar()` both match `name="bar"`.
- **Dropped Intel macOS (`x86_64-apple-darwin`) prebuilt binaries.** macOS releases are Apple Silicon
  only. npm/pip/Homebrew install on an Intel Mac now fails fast with a clear message; build from source
  instead.

### Fixed

- **SSRF denylist + redirect revalidation for web crawler.** Added safeguards to prevent server-side
  request forgery on the `web_scrape` and `web_crawl` tools.
- **Configured embedder preset.** Eliminates cross-process vector state wipe, improving performance
  and cache isolation.
- **Atomic memory writes.** Prevents torn writes on concurrent `memory_put` across sessions.
- **Index panic on corrupted entries.** Fixed a panic when the Fjall index contained malformed keys
  (now degrades gracefully).
- **Git blame robustness.** Improved handling of edge cases (missing commits, detached HEAD, shallow
  repos).
- **Config layer precedence fixes.** MCP > CLI > env > TOML > defaults now strictly enforced.
- **Hot-path allocation cuts.** Reduced allocations in scanner, extract, and query paths; removed
  unnecessary `String::from_utf8` round-trips and clones in the Fjall secondary-index scans.

## [0.3.0] — 2026-06-18

Minor release: `RELEASE_MINOR` bumps 2 → 3, so the blob + index schema versions advance.
Unlike previous minor bumps, the cache is **no longer hard-wiped** — see "Durable cache
refresh" below. The first `basemind scan` / `serve` after upgrading re-extracts in place.

### Added

- **Full MCP↔CLI parity.** Every MCP tool now has a `basemind` CLI subcommand, running the
  identical in-process tool code against the same on-disk index (read-only, no lock — safe
  to run alongside a live `basemind serve`). Human-readable output by default, `--json` for
  the raw structured response. New groups: `basemind query` (outline, symbol, search,
  references, callers, implementations, call-graph, grep, list-files, status, repo-info,
  dependents), `basemind git` (status, history, blame, diffs, churn, symbol-history),
  `basemind memory`, `basemind web`, and `basemind telemetry`.
- **Cache garbage collection + cleanup.** New `cache_gc` (reclaim orphaned blobs no view
  references), `cache_stats` (on-disk size + orphan accounting), and `cache_clear` MCP
  tools, plus `basemind cache gc|stats|clear` CLI commands. GC runs automatically in the
  background on `serve` startup. Blobs are shared across views; GC only sweeps blobs no
  view's index references, under the store lock so it never races a scan.
- **Continuous background watch.** `basemind serve` now watches the working tree and
  incrementally refreshes its index in the background by default (debounced), keeping
  queries fresh without manual `rescan`. Opt out with `basemind serve --no-watch` for very
  large repos / CI.
- **`basemind-cli` skill** documenting the CLI surface, and a restructured README with two
  equal install paths (MCP plugin vs CLI + skill).

### Changed

- **Durable cache refresh on schema bump.** A schema/version bump now refreshes the
  content-addressed blob store **in place** (re-extract overwrites blobs at their stable
  content-hash path; background GC reclaims the remainder) instead of deleting
  `.basemind/blobs/`. No window where the expensive extraction cache is gone. The Fjall
  secondary index (derived, cheap) still rebuilds from blobs on its own schema bump.
- **Git-cache schema** is now tied to `RELEASE_MINOR` so a release refreshes it consistently
  (it auto-rebuilds from git).

### Fixed

- **Tightened token-savings estimate.** The `telemetry_summary` "tokens saved" figure no
  longer scales with total corpus size (it previously credited ~5% of the whole repo per
  grep-style call). Baselines are now derived from the actual response payload, decoupled
  from corpus size.
- **Read-only store degrades gracefully on a schema bump.** A CLI query run before the first
  post-upgrade scan now reads an empty index ("run `basemind scan`") instead of erroring,
  and never opens the stale Fjall index.

## [0.2.6] — 2026-06-18

### Changed

- **Responsive two-line statusline with per-capability metrics.** The status line now
  renders a context row (model · output-style · vim · dir · branch · context%) above the
  basemind row, since a custom statusLine replaces Claude Code's default and cannot sit
  below it. The basemind row breaks telemetry down per capability — searches, git, docs,
  memory, web — showing only buckets with activity today, alongside calls and estimated
  tokens saved. Width comes from `$COLUMNS` with full/compact/minimal tiers; override with
  `BASEMIND_STATUSLINE` and hide the context row with `BASEMIND_STATUSLINE_CONTEXT=0`.

## [0.2.5] — 2026-06-18

### Added

- **PreToolUse guard hook drives basemind usage.** A configurable hook
  (`hooks/pre-tool-guard`) reaches the agent when it reaches for `Grep`/`Glob` and
  points it at the matching basemind tool — the only Claude Code lever that can also
  cover subagents. `BASEMIND_GUARD` selects the mode: `nudge` (default, advisory once
  per session), `redirect` (deny with a pointer to the basemind tool), or `off`.
- **Full-capability awareness in the agent surfaces.** The always-injected MCP
  `get_info` instructions and the `basemind` skill now describe the whole surface —
  code map across 300+ languages, full-text + semantic search, git intelligence,
  document RAG over 90+ file formats, shared memory, and web crawl — instead of a
  code-map-only framing. The README leads with the same capability set.

### Fixed

- **`Store::open` no longer races on a just-released lock.** `acquire_lock` retries the
  exclusive `flock` with a short backoff, fixing a macOS-only flake where a sequential
  open → close → open hit `Locked`.

## [0.2.4] — 2026-06-18

### Added

- **Context-economy operating discipline shipped across every agent surface.** The
  MCP `get_info` instructions, the `basemind` skill, the SessionStart hook, and the
  README now state one default workflow: these tools return paths, line numbers, and
  signatures rather than file bodies, so agents `outline` before reading,
  `search_symbols` / `find_references` / `workspace_grep` before grep, and `rescan`
  after edits. The SessionStart hook injects this every session instead of only
  nudging when the statusline is unset.

## [0.2.3] — 2026-06-18

### Added

- **`serve` auto-scans a fresh repo on startup.** When the working-view index is
  empty, the server kicks off an initial scan in the background (reusing the
  in-process `rescan` path, so it never contends for the Fjall lock). The agent
  no longer has to run `basemind scan` by hand — the statusline flips from
  "scanning…" to live counts on its own.
- **`.basemind/` is now self-ignoring.** The first time the store is created it
  writes `.basemind/.gitignore` containing `*`, so a user's repository never
  accidentally commits the machine-local index. An existing `.gitignore` is left
  untouched.

### Changed

- **README redesign.** Centered hero with a uniform badge row, a fenced
  statusline preview (replacing a broken image reference), and collapsible
  architecture / harness-setup sections. Clarified that registering the
  marketplace and installing the plugin are two distinct steps.

### Fixed

- **Nightly hardening CI is green again.** The `hardening` job built
  `--features full` (kreuzberg's libheif + tesseract stack) but only installed
  protoc, so the `libheif-sys` build failed. The job now installs the codec dev
  libraries and builds libheif 1.23.0 from source (cached across runs).

## [0.2.2] — 2026-06-17

### Added

- **Claude Code plugin now auto-installs the basemind binary.** The MCP launcher
  (`scripts/mcp-launch.sh`) tries version-matched cached binaries, then npx
  (npm package), uvx (PyPI package), and finally downloads the prebuilt binary
  from the GitHub release with checksum verification. Override the launch strategy
  with `BASEMIND_LAUNCHER=auto|npx|uvx|download`.
- **SessionStart hook pre-warms the binary and nudges statusline setup.** The
  hook runs in the background on session start to ensure the first tool call isn't
  a cold install, and suggests `/bm-statusline` if the statusline isn't yet
  configured.
- **`/bm-statusline` slash command wires the statusline into
  `~/.claude/settings.json`.** Plugins cannot set the main statusline
  automatically; `/bm-statusline` is the one-step opt-in.

## [0.2.1] — 2026-06-17

### Fixed

- **Claude Code slash commands now load.** The `/bm` and `/bm-stats` commands
  were under `.claude-plugin/commands/`, which Claude Code ignores — plugin
  component directories must sit at the plugin root, and basemind's marketplace
  `source` is `"./"`, so the plugin root is the repo root. Moved `commands/`
  (and confirmed `skills/`) to the repo root; `.claude-plugin/` now holds only
  the manifests + statusline. The Codex / Cursor / OpenCode trees keep their own
  in-tree copies (their plugin root is their own directory), kept in sync by
  `scripts/sync-plugin-skills.sh`.

### Changed

- **Publish workflow no longer hard-fails on a missing Homebrew token.** The
  GitHub-release-assets job skips the Homebrew tap publisher when
  `HOMEBREW_TOKEN` is absent (and runs it when present), so an optional tap
  update can't fail the binary release.

## [0.2.0] — 2026-06-17

**Minor release — schema wipe (`RELEASE_MINOR=2`).** The blob and inverted-index
formats are versioned to the release minor, so the next `basemind scan` rebuilds
`.basemind/` from source. This is the intentional migration path; no user action
beyond a rescan is needed.

### Changed

- **Dependencies on crates.io.** `kreuzberg` moved from the `v5.0.0-rc.15` git pin
  to the published crates.io release `=5.0.0-rc.18`; the temporary `allow-git`
  exemptions in `deny.toml` are dropped. With no remaining git dependencies,
  `cargo publish` to crates.io is unblocked — basemind now ships on all four
  registries (crates.io, npm, PyPI, Homebrew) from a single tag.
- **README is now a positioning document.** Rewritten from a code-map feature
  inventory into a full AI-context-layer overview: four pillars (Code, Documents,
  Memory, Web), a feature table mapping every MCP tool to its backend, a
  three-flavour quickstart, comparisons against grep / vector-only RAG /
  repo-map indexers / GitHub search, and a differentiators section. Sub-package
  READMEs (npm / PyPI / OpenCode) collapse to install stubs linking the root.
- **Aligned package metadata across all surfaces.** One canonical description and
  a positioning-led 5-keyword set (`mcp`, `agent-context`, `rag`, `code-map`,
  `tree-sitter`) now ship identically across `Cargo.toml`, the npm / PyPI
  manifests, and every harness plugin manifest. GitHub repo description + topics
  updated to match.

### Added

- **Redesigned Claude Code statusline.** Bright 256-color palette (no more
  low-contrast dim text), a `◆` brand mark, a dedicated serve/scan freshness
  dot, and width-aware wide/narrow layouts. The file count now reads the L1 blob
  set directly (exact, deduped) instead of estimating from index bytes; scan
  recency reads the view index mtime; an empty repo renders an actionable
  `run: basemind scan` hint instead of a blank line. The intelligence row
  (documents / memory / web) appears when a LanceDB store is present.
- **Agent-facing skills + slash commands bundled into every plugin tree.** The
  `basemind` routing skill and `basemind-stats` dashboard, plus `/bm` and
  `/bm-stats` slash commands, now ship inside `.claude-plugin/`, `.codex-plugin/`,
  `.cursor-plugin/`, and the `basemind-opencode` npm tarball — so an installed
  plugin teaches the agent how to use the tools without manual onboarding. A new
  `scripts/sync-plugin-skills.sh` (wired as a prek hook) keeps the canonical
  source and the per-harness copies in lock-step.

## [0.1.1] — 2026-06-15

### Fixed

- **`npm-package`** — `npx basemind …` (and equivalent `npm install -g basemind`
  invocations) used to silently fall through to whatever `basemind` already lived
  on `$PATH` because npm skipped creating the `node_modules/.bin/basemind`
  symlink when the `bin:` target was missing at install time (the native binary
  is downloaded by `postinstall`). The wrapper now ships a Node.js shim at
  `bin/basemind.js` that `spawnSync`'s the downloaded native binary, so the
  symlink target exists at install time and npx routes correctly through the
  wrapper.
- **`publish.yaml`** — re-enabled the Homebrew tap step now that
  `HOMEBREW_TOKEN` is on the path to being provisioned. Set the secret on the
  basemind repo before pushing the next tag; goreleaser will then auto-update
  `Goldziher/homebrew-tap`'s `Formula/basemind.rb`.

## [0.1.0] — 2026-06-15

**Initial public release.** First minor wipes the schema (`RELEASE_MINOR=1`)
so any pre-tag `.basemind/` cache rebuilds on next scan — intentional.

Feature-complete code-map server with the kreuzberg document tier surface
(reranker, keywords, NER, summarization, language detection, TOON output) and
schema-driven config across TOML / CLI / MCP / env vars, distributed across
all major coding-agent harnesses (Claude Code, Codex, Cursor, Gemini, Factory
Droid, OpenCode, Copilot CLI, and the generic MCP path) plus npm / PyPI /
crates.io.

### Added

- **`basemind scan`** — rayon-parallel scanner that indexes a workspace into
  content-addressed msgpack blobs (`.basemind/blobs/`) plus a Fjall-backed
  inverted index (`.basemind/views/<view>/index.fjall/`). Two extraction tiers
  ship: L1 outlines (symbols, signatures, imports, docs) and L2 call sites;
  L3 structural hash available for symbol-history diffing.
- **`basemind serve`** — stdio MCP server (`rmcp`) exposing the full code-map
  and git-history tool surface (`outline`, `search_symbols`, `find_references`,
  `find_callers`, `list_files`, `dependents`, `repo_info`, `status`,
  `symbol_history`, `working_tree_status`, `recent_changes`,
  `commits_touching`, `find_commits_by_path`, `diff_file`, `diff_outline`,
  `hot_files`, `blame_file`, `blame_symbol`).
- **Dynamic 300+ language coverage** via
  [tree-sitter-language-pack](https://github.com/kreuzberg-dev/tree-sitter-language-pack).
  Hand-written `.scm` overrides ship for Rust, Python, TypeScript, TSX,
  JavaScript, Go. Other languages for which TSLP ships a vendored `tags.scm`
  (kotlin, csharp, swift, cpp, scala, solidity, lua, …) get best-effort
  symbol + call extraction via the fallback adapter that rewrites
  GitHub-standard `@definition.*` / `@reference.call` captures into
  basemind's `@symbol.*` / `@call.*` shape.
- **Schema-driven config across TOML / CLI / MCP / env vars.** Rust types in
  `src/config/` derive `schemars::JsonSchema`; the snapshot at
  `schema/basemind-config-v1.schema.json` is regenerated from those types and
  asserted by `tests/config_schema.rs`. Adding a config field lights up all
  four surfaces via `#[command(flatten)]` (clap) and `#[serde(flatten)]` (MCP
  params). Precedence: MCP > CLI > env > TOML > defaults, with per-field
  provenance tracking via `src/config/source.rs`.
- **Document tier** — `search_documents` MCP tool over Lance-backed embedding
  index with kreuzberg ingestion. Per-query overrides on every `documents.*`
  and `llm.*` setting, plus `entity_category` and `keywords_contains`
  post-filters.
- **TOON wire format** for MCP responses via
  `documents.output.format = "toon"` (or `--documents-output-format toon`).
  Round-trip parity with JSON asserted in `tests/mcp_smoke.rs`.
- **Language-aware ingestion.** `documents.language.{auto_detect,
  min_confidence, detect_multiple}` flows into kreuzberg's
  `LanguageDetectionConfig` and the chunking tokenizer. ISO 639-3 codes (e.g.
  `"fra"`) surface on `FileMapDoc.detected_languages`.
- **Cross-encoder reranker** as a post-step on `search_documents`. Off by
  default; `documents.reranker.{enabled,preset,top_k}` opts in. Preset is
  validated upfront against `kreuzberg::get_reranker_preset`; reranked index
  is bounds-checked before reorder.
- **Keyword extraction (YAKE / RAKE) and named entity recognition** at
  extract time. New tail fields on `FileMapDoc` (`keywords`, `entities`)
  with `#[serde(default)]` — blob-compatible with prior blob shapes
  (asserted by `tests/schema_bump.rs`).
- **Extractive + abstractive summarization** via `documents.summarization`.
  Abstractive routes through liter-llm with the new top-level `[llm]`
  section (model in `provider/model` form, api_key, base_url, temperature,
  timeout, retries, max_tokens). NER backend `llm` now wires the resolved
  `LlmConfig`.
- **`SecretString` newtype + `ApiKey` enum** (`Literal | Env | Unset`).
  Secrets mask to `"<redacted>"` in `Debug` / `Display` and across
  `Serialize` (including the toml→serde_json validation round-trip).
- **Real-OSS hardening harness** (`tests/harden.rs`, `./scripts/harden.sh`)
  — clones 8 upstream repos (ripgrep, tokio, typescript, react, django,
  requests, gin, ripgrep-shallow), exercises every MCP tool against each,
  and pins canary lower bounds (tokio: `find_references("spawn") >= 200`,
  django: `find_references("get") >= 200`,
  react: `search_symbols("useState") >= 20`,
  ripgrep-shallow truncation surfaces).
- **Schema sync to release version** — `RELEASE_MINOR` in `src/version.rs`
  drives both `INDEX_SCHEMA_VER` and the blob `SCHEMA_VER`. Minor-release
  bumps wipe `.basemind/` on next scan; patch releases stay compatible.
- Distribution: `brew install Goldziher/tap/basemind`,
  `npm install -g basemind`, `pip install basemind`,
  `cargo install basemind --locked`. Precompiled binaries on GitHub
  Releases for `{x86_64,aarch64}-{linux-gnu,apple-darwin}` and
  `x86_64-pc-windows-gnu`.
- **Per-harness install matrix** — manifests + skill bundle for every major
  coding-agent harness, all bumped in lock-step by
  `task release:sync-version VERSION=…`:
  - **Claude Code** — `.claude-plugin/plugin.json` + `marketplace.json` +
    `statusline.sh` (live one-line summary of the indexed map with true-color
    brand mark, freshness dot, and call/token-savings telemetry from
    `.basemind/telemetry.jsonl`). Install via
    `/plugin marketplace add Goldziher/basemind` then
    `/plugin install basemind@basemind`.
  - **Codex CLI / App** — `.codex-plugin/plugin.json` with full `interface`
    block (`displayName`, category `Developer Tools`, `capabilities`,
    `defaultPrompt`, `brandColor`). `scripts/sync-to-codex-plugin.sh` mirrors
    into a fork of the `openai/plugins` marketplace.
  - **Gemini CLI** — root-level `gemini-extension.json` with `contextFileName`
    and `mcpServers`. Install via
    `gemini extensions install https://github.com/Goldziher/basemind`.
  - **Cursor** — `.cursor-plugin/plugin.json`.
  - **OpenCode** — published as the
    [`basemind-opencode`](https://www.npmjs.com/package/basemind-opencode)
    npm package. Install via
    `{ "plugin": ["basemind-opencode@latest"] }` in `opencode.json`. Skills
    are bundled into the tarball; the plugin shim does dual-mode resolution
    (bundled-path-first, repo-root-fallback) so monorepo dev and npm install
    both work without duplication.
  - **Factory Droid / GitHub Copilot CLI** — reuse the existing
    `.claude-plugin/marketplace.json` per their published patterns.
  - Every manifest carries the same canonical user-facing description and
    crates.io-capped 5-keyword set (`mcp`, `tree-sitter`, `code-map`,
    `scanner`, `indexer`).
- **Release pipeline** — `.github/workflows/publish.yaml` publishes on
  every `v*` tag: GitHub release assets (cross-compiled binaries via
  goreleaser + zig), crates.io, npm × 2 (`basemind` binary wrapper +
  `basemind-opencode` OpenCode plugin), and PyPI. Per-registry idempotent
  via existing-version detection; OIDC-driven trusted publishers on all
  four registries.

### Changed

- `tree-sitter-language-pack`: `=1.9.0-rc.27` → `=1.9.0-rc.45`. Hierarchical
  `data_extraction` + 17 data formats; rc.40–45 are CI/codegen-only.
- `kreuzberg` bumped to the reranker / LLM API surface
  (`=5.0.0-rc.7` baseline → published rc covering reranker + LLM at publish
  time).
- `alloc-stdlib = "=0.2.2"` pin lifted (no longer binding after lock
  re-resolve; single `alloc-no-stdlib 2.0.4` in the tree).
- `src/mcp/types.rs` + `src/mcp/helpers.rs` split into `_documents.rs`
  siblings to stay under the 1000-line cap.

### Performance

- Harden 8/8 green across ripgrep / tokio / typescript / react / django /
  requests / gin / ripgrep-shallow. All canaries pass. Per-repo scan times
  within baseline: typescript 21.7 s (81 324 files), tokio 0.2 s (859
  files), django 2.5 s (7 061 files), react 2.2 s, requests 0.7 s, gin
  1.0 s, ripgrep 4.0 s, ripgrep-shallow 0.16 s. All 25 MCP tools clean
  across all repos.
- `search_documents` post-processing releases the store read-lock before
  blob I/O; `ahash::AHashMap` / `AHashSet` on the post-filter path.

[0.5.0]: https://github.com/Goldziher/basemind/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/Goldziher/basemind/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/Goldziher/basemind/compare/v0.2.6...v0.3.0
[0.2.6]: https://github.com/Goldziher/basemind/compare/v0.2.5...v0.2.6
[0.2.5]: https://github.com/Goldziher/basemind/compare/v0.2.4...v0.2.5
[0.2.4]: https://github.com/Goldziher/basemind/compare/v0.2.3...v0.2.4
[0.2.3]: https://github.com/Goldziher/basemind/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/Goldziher/basemind/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/Goldziher/basemind/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/Goldziher/basemind/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/Goldziher/basemind/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/Goldziher/basemind/releases/tag/v0.1.0
