# Architecture

basemind is a single Rust crate that builds one binary (`basemind`) and exposes
its internals as a library. The binary serves three roles: `basemind scan` indexes
a workspace, the daemon (`basemind comms daemon`, auto-spawned; singleton per user) is the sole
writer managing a machine-global cache and hosts the MCP router over one shared read stack per
workspace, and `basemind serve` is a thin stdio relay that ensures the daemon and pumps bytes to it
(HTTP-native clients dial the daemon directly). `serve` needs the `comms` feature; the one-shot CLI
(`basemind code …`, `git …`) opens the store itself and does not.

The Cargo workspace also vendors four crates under `crates/` — `stack-graphs`,
`tree-sitter-graph`, `tree-sitter-stack-graphs`, `lsp-positions` — a tree-sitter-0.26 fork of
the upstream stack-graphs project, maintained in-tree because upstream targets an older
tree-sitter. The fork root-caused and fixed two upstream panics and drops the C-FFI, serde,
sqlite, and visualization modules basemind doesn't use. It backs the Python/Java half of the
code-intelligence tier (see [Code intelligence](#code-intelligence-precise-resolution) below).

> This document describes the system as-built. For *why* a given decision was made and which
> alternatives were rejected, see the Architecture Decision Records in [`adr/`](adr/README.md).

```text
                    ┌─────────────┐
                    │ basemind    │
                    │ scan        │
                    └─────┬───────┘
                          │ extraction results over IPC (UDS)
                          ▼
        ┌──────────────────────────────────────┐
        │  basemind daemon (singleton)         │
        │                                      │
        │  ┌─────────┐ ┌─────────────┐         │
        │  │ Fjall   │ │ LanceDB     │         │
        │  │ writer  │ │ writer      │         │
        │  └─────────┘ └─────────────┘         │
        │  sole writer per machine; hosts the  │
        │  MCP router + one shared read stack  │
        │  per workspace                       │
        └────┬─────────────────────────┬───────┘
             │ <cache>/cache/          │ streamable HTTP (opt-in)
             │ workspaces/<key>/       │ and the stdio relay
   ┌─────────▼──────┐                  │
   │ Content-addr   │                  │
   │ msgpack blobs  │       ┌──────────┴───────────────┐
   │ (cache/blobs,  │       │ basemind serve           │
   │ machine-wide,  │       │ (stdio relay, one per    │
   │ deduped)       │       │ client session)          │
   └────────────────┘       └──────────┬───────────────┘
                                       │ MCP stdio
                                       ▼
                            ┌──────────────────────┐
                            │ AI coding agents     │
                            │ (Claude Code, etc.)  │
                            └──────────────────────┘
```

## Source layout

```text
src/
├── lib.rs                  — public re-exports
├── main.rs                 — CLI entry (scan, serve, watch, code, lang, …)
├── version.rs              — RELEASE_MINOR — single source of truth for schema versions
├── scanner.rs              — orchestrates the scan pipeline: extraction → Fjall index →
│                             code-map persist barrier → optional enrichment lanes
├── scanner_file.rs         — per-file pipeline (read/classify/extract) + the rayon pool; routes
│                             non-code files (`lang::is_non_code`) to the document tier
├── scanner_filter.rs       — include/exclude globs, submodule pruning, incremental
│                             IndexFilter (nested-.gitignore-aware)
├── scanner_lanes.rs        — fault containment (catch_unwind) for the lanes that run after
│                             the code-map persist barrier
├── scanner_code.rs         — code-search branch: L1/L2 → code chunks (feature code-search)
├── scanner_docs.rs         — document-tier scan (PDF/Office/HTML → LanceDB, feature documents); also takes
│                             prose/data/config (md/json/yaml/toml, `lang::is_non_code`): the code map is code-only
├── scanner_policy.rs       — embed-policy reconciliation: vector rows follow `embed*` config changes
├── scanner_candidates.rs   — candidate enumeration + the [scan] max_candidates ceilings and
│                             the BASEMIND_ALLOW_EXTRA_ROOTS grant for extra_roots
├── scanner_index_batch.rs  — per-worker index write batch: commits on files OR staged bytes
│                             OR the process-wide staging ceiling
├── scanner_drive.rs        — ScanObserver: per-file results are streamed, never accumulated
├── chunk_drive.rs          — bounded process/absorb driver for whole-corpus passes
├── scan_evidence.rs        — scan-inflight.json breadcrumb + rss/ceiling log line: what a
│                             SIGKILLed scan leaves behind (see "Evidence" below)
├── backpressure.rs         — FootprintGate: best-effort memory admission control;
│                             DocSlot semaphore for max_concurrent_documents
├── sysres.rs               — process memory usage + platform limit (cgroup v1/v2, mach,
│                             Windows); rate-limited, backs the footprint ceiling
├── sysres_cgroup.rs        — cgroup / procfs parsers + mount-parameterised readers
├── alloc_tag.rs            — macOS: retag mimalloc's mappings (Mach VM tag 254) so the heap stops
│                             reading as IOAccelerator "GPU" memory
├── daemon_lock.rs          — single-owner daemon lock, pidfile, machine-wide live-daemon registry +
│                             ceiling (BASEMIND_MAX_DAEMONS)
├── eval/                   — `basemind admin eval`: scores retrieval quality + token savings
│                             against gold task files (see benchmarks/eval/README.md)
├── chunk.rs                — code-chunk model + chunker (feature code-search)
├── embeddings.rs           — shared ONNX embedding engine (feature intelligence)
├── store.rs                — content-addressed msgpack blob store; Store facade; holds IndexDb
├── store_blob.rs           — blob (de)framing + atomic write
├── store_layout.rs         — cache root / workspace-dir mapping; workspace.json marker
├── store_lock.rs           — workspace-cache `.lock` flock + holder metadata + writer probe
├── store_gc.rs             — tier-aware, cross-workspace reference-counted blob sweep (`cache gc`)
├── store_gc_live.rs        — liveness split by tier (code lane vs document lane) + the machine-wide
│                             `cache/gc.lock` that serialises sweeps
├── store_gc_budget.rs      — cache size budget + `gc-state.json` (last sweep health)
├── store_gc_workspace.rs   — orphaned-workspace reaper for the machine-global cache
├── store_seed.rs           — seed a new linked worktree's view from a sibling checkout (reflink)
├── store_cache_admin.rs    — cache clear / stats admin surface (CLI + MCP)
├── index/
│   ├── mod.rs              — Fjall-backed secondary index; INDEX_SCHEMA_VER; refs_by_def /
│   │                         refs_by_path (code-intelligence reverse index)
│   ├── keys.rs             — length-prefixed composite key encodings
│   ├── grep_bloom.rs       — per-file trigram bloom rows: the `code_grep` candidate prefilter
│   │                         (ADR-0012)
│   ├── name_dict.rs        — resident dictionary of distinct callee / trait names behind
│   │                         `references` / `callers` / `implementations`
│   └── writer.rs           — atomic read-before-write upsert; per-file commit;
│                             upsert_cross_file_edge
├── extract/                — tree-sitter extraction tiers
│   ├── l1.rs               — outlines (symbols, signatures, imports, docs)
│   ├── l2.rs               — call sites (callee, byte offset, line/col)
│   ├── l3.rs               — structural hash of symbol bodies
│   ├── locals.rs           — tree-sitter `locals`-query intra-file scope resolution
│   │                         (the grammar-native fallback for the code-intel tier)
│   ├── doc.rs              — xberg integration; FileMapDoc (+ keywords,
│   │                         entities, summary on the documents path)
│   ├── doc_tiers.rs        — the optional document tiers (documents-ocr, documents-ner) and what a
│   │                         build without them reports
│   └── doc_cost.rs         — header-derived peak-memory estimate of one document (skip if it
│                             alone exceeds the footprint ceiling)
├── intel/                  — code-intelligence tier: scope/import-resolved navigation
│   │                         (see "Code intelligence" below)
│   ├── mod.rs              — engine dispatch + feature gates
│   ├── model.rs            — FileResolvedRefs / ResolvedEdge / ImportEdge / ExportEdge
│   ├── resolve.rs          — per-file dispatch: oxc → stack-graphs → locals fallback
│   ├── resolve_pass.rs     — post-scan pass: caching, index staging, cross-file trigger
│   ├── resolver.rs         — per-language SpecifierResolver (JS/TS, Python, Java)
│   ├── xfile.rs            — cross-file stitch (importer binding → export site)
│   ├── js.rs               — JS/TS via oxc (feature code-intel-js)
│   ├── stackgraph.rs       — Python/Java via vendored .tsg stack-graphs
│   │                         (feature code-intel-stack)
│   └── tsg/{python,java}.tsg — vendored stack-graph name-binding rulesets
├── git_history/            — precomputed git-history index (`git-history.fjall/` in the workspace cache,
│   │                         see "Git-history index" below)
│   ├── mod.rs              — GitHistoryIndex; Local/Remote backend; CommitMeta; partitions
│   ├── builder.rs          — sync: walk HEAD, populate partitions incrementally
│   ├── reader.rs           — indexed-vs-live query surface for the history MCP tools
│   ├── fts.rs              — full-text search over commit messages
│   ├── keys.rs, encoding.rs — key layout + delta-varint posting-list encoding
│   ├── proto.rs            — daemon RPC request/response shapes
│   └── remote.rs           — daemon-forwarding backend (feature comms)
├── config/                 — schema-driven config (TOML/CLI/MCP/env)
│   ├── v1.rs               — top-level ConfigV1, LlmConfig, CodeIntelConfig (schemars-derived)
│   ├── resources.rs        — [resources]: scan_threads, embed_threads, max_footprint_mb, max_map_cache_mb
│   │                         (MaxFootprint: integer | "auto" | "off"), document_models
│   │                         (see "Resource governance" below)
│   ├── root_guard.rs       — workspace-root allow-list: RootRefusal, the resolved-path
│   │                         verdict, BASEMIND_ALLOW_ANY_ROOT, root-admission.json
│   ├── documents.rs        — DocumentsConfig + sub-configs, ApiKey, SecretString
│   ├── code.rs             — [code_search] config table
│   ├── comms.rs            — [comms] config table (broker daemon + identity)
│   ├── overrides.rs        — DocumentsCliOverrides — backs the clap flags
│   ├── layered.rs          — merge_layers (Mcp > Cli > Env > File > Default)
│   └── source.rs           — ConfigSource + ProvenanceMap ledger
├── mcp/                    — MCP server: 15 tools, one per job, each with its own exact
│                             schema (ADR-0018), plus the mode-dispatched `agents` tool
│                             in `comms` builds — see "MCP surface" below
│   ├── mod.rs, state.rs    — server bootstrap; shared ServerState
│   ├── types_tools.rs      — wire arguments: one deny_unknown_fields struct per tool, and
│   │                         VERB_TOOLS, the tool names in declaration order
│   ├── strict.rs           — Strict<T>: exact argument validation; an unknown or malformed
│   │                         argument becomes an isError result naming the closest accepted
│   │                         field and the accepted set
│   ├── tool_contract.rs    — the contract every tool shares: answer(), isError results that
│   │                         name the fix (not indexed, empty index, feature compiled out),
│   │                         the default max_tokens budget, the alwaysLoad meta and schema
│   │                         compaction applied to every advertised definition
│   ├── mode.rs             — define_mode!: the `mode` vocabulary and per-mode field
│   │                         validator of `agents` only
│   ├── tools.rs            — #[tool] shims of code_outline, code_symbols, code_read,
│   │                         code_grep, code_search, code_refs, find_files (thin wrappers;
│   │                         1000-line cap)
│   ├── tools_<area>.rs     — the remaining shims: git (git_log, git_blame, git_diff),
│   │                         memory (docs_search, memory_save, memory_search), admin
│   │                         (index_status, index_rescan), comms (→ `agents`)
│   ├── tasks.rs            — Tasks extension: results are inline; index_rescan, code_search
│   │                         and docs_search become a task only if still running after 3 s
│   ├── term_index.rs       — resident term index (symbol names + import strings) behind
│   │                         `code_symbols` and the `code_refs` importer lookup
│   ├── index_route.rs      — where a `code_refs` call-site/caller/implementation read runs: local index,
│   │                         daemon-hosted (in-process), forwarded to the daemon, or in-RAM fallback
│   ├── index_read.rs       — daemon-side executor for the forwarded index reads (`IndexRead`)
│   ├── l1_cache.rs         — byte-budgeted read-through cache of decoded outlines (max_map_cache_mb)
│   ├── helpers.rs          — tool bodies; shared scan / decode helpers
│   ├── helpers_<area>.rs   — the tool bodies, area-sliced: admin (index_status,
│   │                         index_rescan), calls, calls_scan, code, code_search (feature
│   │                         code-search), comms, documents, expand (code_read), files,
│   │                         fingerprint, git, git_file, git_show, grep, impls, intel (the
│   │                         code_refs definition body), memory, page (offset paging for
│   │                         the ranked code_search / docs_search), registry, telemetry
│   ├── memory.rs           — search_documents + memory_* bodies behind docs_search,
│   │                         memory_save and memory_search
│   ├── types.rs, types_<area>.rs — internal request shapes and response structs of the
│   │                         bodies (code, documents, git, impls, memory, comms, registry);
│   │                         the wire arguments are in types_tools.rs
│   ├── cursor.rs           — cursor encoding for paginated tools
│   ├── savings.rs          — token-savings heuristics, keyed by tool name (rows recorded
│   │                         under the old domain:mode keys are mapped to it)
│   ├── telemetry.rs        — per-call telemetry.jsonl writer; the key is the tool name
│   └── budget.rs, toon.rs, lean.rs, lenient.rs, notifications.rs,
│       completions.rs, prompts.rs, tokens.rs, background.rs, daemon_forward.rs,
│       map_fingerprint.rs, identity.rs — response budgeting, TOON/lean output
│       shaping, request leniency, daemon-forward plumbing for Seam B
├── stdio_relay.rs          — protocol-aware stdio relay behind `basemind serve`; survives a
│                             daemon replacement without closing the host's pipes
├── comms/                  — agent-comms daemon: broker, thread registry, memory,
│                             worktree registry (see "Agent comms" below); workspace_pool.rs is
│                             the daemon's hot-workspace + warm-read-stack pool, index_read_proto.rs
│                             the wire types of the forwarded index reads;
│                             http_auth.rs is the BASEMIND_ALLOW_HTTP opt-in + the bearer
│                             token every HTTP front-end request must present
├── git/, git_cache.rs      — gix-backed history, blame, diff, status (git/mod.rs,
│                             commit.rs, remote.rs, worktree.rs); git_cache.rs is the
│                             blame/log/diff disk + in-process LRU cache
├── query.rs                — read-side helpers shared by MCP tools + CLI
├── path.rs                 — RelPath: byte-precise repo-relative paths
├── lang.rs                 — LangId = &'static str (TSLP pack name), parser pool,
│                             query cache, override-then-TSLP-fallback try_get_query;
│                             is_non_code(): the grammars routed to the document tier
├── lance/                  — LanceDB schema + open/write helpers (feature intelligence)
├── search/                 — BM25 / exact / RRF keyword search over code chunks
│                             (feature code-search)
├── registry/                — daemon-side workspace/worktree/branch registry
├── cli/                     — CLI subcommand groups: admin, agents (daemon-agent verbs),
│                             code, git, memory, registry (the CLI-only `workspace` listings). Every
│                             MCP tool has a command; shorthands and administrative
│                             commands have no tool;
│                             comms_daemon.rs is `basemind comms` — daemon lifecycle only
│                             (daemon/start/stop/status/doctor)
│                             choices.rs holds the clap value enums, exit.rs the exit-code
│                             contract (0/1/2/3/4/130), interrupt.rs the Ctrl-C scan cancel,
│                             context.rs the one-shot server (daemon-forwarding when one is up)
├── doctor_cli.rs, lang_cli.rs, comms_cli.rs — `doctor`, `lang`, and `comms`/`daemon`/`statusline`
│                             entry points in the binary crate
├── comms_stop.rs           — `comms stop` and `comms stop --all`: ask, wait, reclaim
├── queries/<pack-name>.scm — hand-written extraction queries (override TSLP tags.scm)
├── render.rs, hashing.rs, watcher.rs                   — supporting modules

crates/                    — vendored tree-sitter-0.26 stack-graphs fork (workspace members):
                             stack-graphs, tree-sitter-graph, tree-sitter-stack-graphs,
                             lsp-positions
```

## Scan pipeline

```text
Walker (gitignore-aware)
  → filter by user glob + size cap
  → rayon par_iter
    → process_file(rel, contents):
        lang::detect()                — TSLP extension → LangId; prose/data/config grammars
                                        (lang::is_non_code) and unrecognised files go to the
                                        document tier (scanner_docs), not the code map
        L1 outline   (always)         — extract::l1
        L2 calls     (eager if cfg)   — extract::l2
        Store::write_l1               — content-addressed msgpack blob
        Store::write_l2 (if eager)
        grep bloom row (working tree) — built from the bytes already in hand
  → collect FileResult { rel, l1_hash, l2_hash?, … }
  → Forward to daemon via IPC (UDS)
        IndexWriter::upsert_file(...) — Fjall secondary index
        per-file commit               — atomic batch (daemon-side)
  → apply_outcomes:
        write Index meta
        prune deleted files via IndexWriter::remove_file
  → flush_code_map: Store::flush → index.msgpack        ═══ DURABILITY BARRIER ═══
  → optional enrichment lanes (each run_optional_lane-wrapped, catch_unwind-contained):
        LANE_RESOLVE        — intel::resolve_pass (intra + cross-file resolution)
        LANE_DOC_BATCHES    — document chunks → LanceDB (feature documents)
        LANE_CODE_BATCHES   — code chunks → LanceDB (feature code-search)
        LANE_CODE_REMOVALS  — purge code_chunks rows for removed files
        LANE_DOC_REMOVALS   — purge documents rows for removed files
        LANE_BM25_STATS     — recompute corpus-global BM25 N / avgdl
```

The daemon is the sole writer to Fjall and LanceDB. Scan processes forward their extraction
results to the daemon, which applies index updates atomically.

Key invariants:

- **Per-file commit** — every `process_file` commits its Fjall batch before
  returning. Fjall handles cross-thread locking; the scanner does not.
- **Atomic upsert** — `IndexWriter::upsert_file` is read-before-write: read
  existing primary entries first to derive secondary-index keys for deletion,
  then stage all deletes + inserts in one batch. No torn state on re-scan.
- **Code-map persist barrier** — `Store::flush` (writing `index.msgpack`) runs
  immediately after extraction and the Fjall writes are done, BEFORE any optional
  lane. Everything after the barrier — resolved-reference stitching, LanceDB
  document/code embedding batches, BM25 corpus stats — is enrichment: each lane
  runs inside `scanner_lanes::run_optional_lane` (`catch_unwind`-wrapped), so a
  panic or hang in one lane degrades only that lane (e.g. "no resolved refs")
  and never costs the code map itself. Source: `src/scanner_lanes.rs`.
- **Code-only code map** — `code_symbols`, `code_outline`, `code_grep` and `code_refs` see code only.
  Markdown, rst, asciidoc, vimdoc, csv, json, yaml, toml, ini, xml, properties, dotenv, diff,
  gitignore/gitattributes and fluent files are chunked and searched as documents (gated by
  `[documents] include` / `exclude` and the MIME allowlist). A path that changes tier is moved on the
  next full or incremental scan: the stale code-map rows are purged, and the stale blobs are reclaimed
  by the tier-aware sweep (see [Blob store and GC](#blob-store-and-gc)). A build without the
  `documents` feature keeps the grammars, since there is no other tier for those files to go to.
- **Document feature tiers** — `documents` extracts text from PDFs (text layer), Office formats,
  spreadsheets, email, HTML, XML, Markdown/MDX and Jupyter notebooks, with keywords, extractive
  summaries, the reranker and embeddings. `documents-ocr` adds Tesseract OCR (raster images, PDFs
  with no text layer), `documents-ner` adds ONNX named-entity recognition (`[documents.ner]`, the
  `entity` filter of `docs_search`), and `documents-ocr-paddle` adds PaddleOCR as a fallback stage for source
  builds. `full` enables the first three. Without `documents-ocr` a raster image is skipped like a
  binary and a PDF with no text layer is indexed empty with a warning; without `documents-ner` a
  `docs_search` with `entity` is an error and `[documents.ner] enabled = true` logs a warning. Archive
  extraction and the LLM-backed summarization and NER backends are not compiled into any build.
  Source: `src/extract/doc_tiers.rs`.
- **Bounded scan** — per-file results are streamed to the index in chunks (`scanner_drive.rs`), never
  accumulated; the footprint gate pauses the drive between chunks and shrinks it when it cannot
  hold the ceiling (see [Resource governance](#resource-governance)).
- **Eager L2 cost** — scanning TypeScript at ~81 k files takes ~22 s with
  eager L2 on (the default). The `scan.eager_l2 = false` escape hatch trades
  reference search for fastest scan.
- **`scan_paths` removal mirror** — when a file disappears between scans,
  `scan_paths` calls `IndexWriter::remove_file` so secondary indexes don't leak
  stale entries.

## Code intelligence (precise resolution)

basemind's default navigation (`code_refs`) is a name-based scan over
the tree-sitter code map: fast and complete, but it can't tell a shadowed local from an import
or a same-named symbol in another scope. The `intel` tier (`src/intel/`) adds a second,
precision layer: per-ecosystem engines that run their own parser, resolve scope and imports
properly, and produce resolved reference/definition edges the scanner's post-scan pass
persists into the Fjall `refs_by_def` reverse index.

### Two engines

| Ecosystem | Engine | Feature | Coverage |
|---|---|---|---|
| JavaScript / TypeScript | oxc (`oxc_semantic` + `oxc_resolver`) | `code-intel-js` | Full scope + import/export resolution, intra- and cross-file. Self-contained — no tree-sitter grammar needed. |
| Python, Java | vendored `.tsg` stack-graphs (`crates/stack-graphs`) | `code-intel-stack` | `.tsg` name-binding rules executed against the tree-sitter parse tree, per file, for intra-file resolution; cross-file resolution runs through the shared `SpecifierResolver` join (functions/classes/imports for both languages). Java qualified member calls are a known gap (see below). |
| Every other language | tree-sitter `locals` query (`src/extract/locals.rs`) | none (always on) | Intra-file lexical scope binding only — the grammar-native fallback, no import resolution. |

`[code_intel] precise_resolution` (default `true`) is the master switch: when `false`, the
oxc/stack-graphs engines are skipped and every language falls back to `locals`.

### Intra-file vs. cross-file

- **Intra-file resolution** links a use to its definition within the same file (a `ResolvedEdge`:
  `use_start..use_end` → `def_start..def_end`, byte spans). Computed per file by
  `intel::resolve::resolve_file` and cached in a content-addressed `<hash>.rref.msgpack` blob
  (`intel::model::FileResolvedRefs`) alongside the L1/L2 blobs — a file whose bytes are
  unchanged skips re-analysis on the next scan.
- **Cross-file resolution** links an importer's local binding to the definition it actually
  refers to in another file. `intel::xfile::stitch_cross_file_edges` runs once per scan: for
  each importer, it resolves the module specifier via a per-language
  `intel::resolver::SpecifierResolver` (JS/TS: Node/tsconfig resolution via `oxc_resolver`,
  including monorepo path aliases; Python: dotted/relative import path arithmetic over `src/`
  and flat layouts; Java: fully-qualified-name resolution over Maven/Gradle source roots), then
  joins the imported name against the target file's export list — following re-export chains
  (package `__init__.py`, TS barrels) up to 8 hops. The result is written straight to
  `refs_by_def` / `refs_by_path`; nothing cross-file is cached in the per-file blob, so an
  unchanged file whose *dependency* moved still gets re-stitched correctly.
- **Probe index.** The stitch also records what each importer depended on. `import_targets` keeps
  the stitch result per import binding, and `importers_by_probe` maps every path the resolver
  consulted to the importers that consulted it: hit targets, every re-export hop file, failed
  candidates (the `util.ts` tried before `util.js`, a missing `pkg/__init__.py`) and the
  `tsconfig.json` / `package.json` files it read. JS/TS probes are captured by wrapping oxc's
  `FileSystem` (the resolver's cache is cleared per importer so every probe reaches the recorder);
  Python and Java record each candidate path their path arithmetic tests.
- `intel::resolve_pass` has two entry points sharing this compute/stage machinery: `resolve_pass`
  (wholesale, after a full `scan`) and `resolve_pass_incremental` (the watcher path). The
  incremental pass restages the changed files' intra facts, then re-stitches the *changed paths
  plus a prefix scan of `importers_by_probe`* for each changed, added or deleted path (and each of
  its ancestor directories, since the resolver does not descend into a directory it found
  missing). That set is exact, including re-export chains and shadowing files, and O(affected):
  no blob outside it is read. An index without the `import_probe_ver` stamp in `meta` (created
  before the probe keyspaces, or a wholesale pass that did not finish) runs the wholesale pass
  once instead.

### The honest contract: name scan is the floor, resolution only annotates

`code_refs` with a `path` (callers of the definition in that file) never narrows to "only the
resolved subset" — it reports every call site the same call without `path` would (the same
name-based, no-scope scan), so the two agree on `total` for an unambiguous name; that set is the
answer to "what calls this?" and is complete unless truncated. Resolution *annotates* each hit with
`resolved: Option<bool>` (`true` = proven to bind to this definition) and reports `resolved_total`
on the response — a call without `path` carries no `resolved` annotation, since it never resolves a
definition to check against. `resolved: false`
is not evidence a hit isn't a real caller — resolution can't see through a module-object import
(`from pkg import mod` then `mod.f()`) or an unresolvable path alias — so a caller wanting
completeness should trust `total`, not `resolved_total`; a caller wanting precision filters on
`resolved`. `code_refs` with `relation: "definition"`
(`src/mcp/helpers_intel.rs`) is the direct read surface over this tier: it resolves a
`path:line:column` position (1-based) to its definition, following one cross-file hop when the in-file
binding is an import, and returns no `definition` (not an error) when the position holds no
resolved binding.

### Known limitation

Java cross-file member-call resolution (`Foo.greet()`, where `Foo` is imported) is not yet
resolved to the imported class's method — only same-file, non-conflated intra resolution is
covered today (see the `java_imported_class_not_conflated_with_local_method` test in
`src/intel/stackgraph.rs`). Java static imports (`import static a.b.C.method;`) are also not
distinguished from type imports and so never cross-file-resolve (`src/intel/resolver.rs`).
Both are misses, never wrong answers — the name-based scan still finds these call sites.

## Inverted index

A Fjall LSM database at `<cache>/cache/workspaces/<workspace_key>/views/<view>/index.fjall/`
(`<cache>` is `BASEMIND_DATA_HOME`, default `~/.local/share/basemind` on Linux). Fjall admits one
process per directory, so the daemon holds it and sessions reach it through the daemon (see
[Index reads](#index-reads-and-resident-lookup-structures)). Source: `src/index/{mod,keys,writer}.rs`.

| Keyspace | Purpose |
|---|---|
| `meta` | Constants (e.g. `schema_ver`). |
| `symbols_by_path` | Per-file outline lookups. |
| `symbols_by_name` | `name`-prefix range scans for symbol search. |
| `calls_by_path` | Per-file call lookups. |
| `calls_by_callee` | `callee`-prefix range scans — drives the `code_refs` call-site lookup. |
| `imports_by_module` | `module`-prefix range scans — drives `code_refs` with `relation: "importers"`. |
| `imports_by_path` | Per-file import lookups. |
| `implementations_by_trait` | `trait`-prefix range scans — drives `code_refs` with `relation: "implementations"`. |
| `implementations_by_path` | Per-file implementation lookups. |
| `refs_by_def` | Scope/import-resolved reference edges keyed by defining site — backs the `resolved` annotation on `code_refs` caller hits and the cross-file hop of `relation: "definition"`. |
| `refs_by_path` | `refs_by_def` companion keyed by the USE file — O(prefix) delete on re-resolve. |
| `import_targets` | Stitch result per import binding (importer + binding offset → definition), plus one reserved probe-manifest row per importer; O(prefix) delete on re-stitch. |
| `importers_by_probe` | Probed path hash → importers whose resolution consulted it; the affected-importer lookup of the incremental re-stitch. |
| `code_bm25_postings` | Code-search BM25 keyword postings (feature `code-search`). |
| `code_bm25_by_path` | Per-file companion of `code_bm25_postings` — O(prefix) delete on re-index. |
| `grep_bloom` | One trigram bloom filter per indexed file, keyed by path, with the `(size, mtime_ns)` it was built under; the `code_grep` prefilter ([ADR-0012](adr/0012-grep-content-prefilter.md)). |
| `embeddings` | Reserved for in-Fjall vector index; LanceDB owns the live vectors. |

The user-authored keyspaces are NOT in the per-view database. They live in a per-repo-group Fjall
database at `<cache>/cache/repos/<group>/memory.fjall/` (see [Split memory](#split-memory)):

| Keyspace (in `memory.fjall`) | Purpose |
|---|---|
| `memory_by_key` | Agent memory (`memory_save` / `memory_search`); LanceDB owns the embeddings. |

A store written before the audit and mining features were removed also holds `memory_archive` and
`proposals`. They are never opened and never deleted.

Key shapes (length-prefixed, see `src/index/keys.rs`):

```text
symbols_by_path     u16:len(rel) ‖ rel ‖ start_byte:u32_be
symbols_by_name     u16:len(name) ‖ name ‖ kind:u8 ‖ u16:len(rel) ‖ rel ‖ start_byte:u32_be
calls_by_path       u16:len(rel) ‖ rel ‖ start_byte:u32_be
calls_by_callee     u16:len(callee) ‖ callee ‖ u16:len(rel) ‖ rel ‖ start_byte:u32_be
imports_by_module   u16:len(module) ‖ module ‖ u16:len(rel) ‖ rel ‖ start_byte:u32_be
refs_by_def         u16:len(def_path) ‖ def_path ‖ def_start:u32_be ‖ u16:len(use_path) ‖ use_path ‖ use_start:u32_be
refs_by_path        u16:len(use_path) ‖ use_path ‖ use_start:u32_be ‖ u16:len(def_path) ‖ def_path ‖ def_start:u32_be
import_targets      u16:len(importer) ‖ importer ‖ local_start:u32_be  →  u16:len(def_path) ‖ def_path ‖ def_start:u32_be
                    (local_start = u32::MAX is the manifest row: the importer's probe hashes, 8 bytes each)
importers_by_probe  blake3(probed_path)[..8] ‖ importer  →  (empty)
grep_bloom          rel (the path bytes)  →  version:u8 ‖ size:u64 ‖ mtime_ns:i64 ‖ bloom bits
```

Length-prefixed components guarantee prefix-scan isolation: a `Foo` prefix never
spills into `Foobar`. Schema version is stamped in the `meta` keyspace; mismatch
on open drops the whole `index.fjall/` directory and the next scan rebuilds it.

### Index reads and resident lookup structures

The `code_refs` call-site, caller and implementation lookups scan `calls_by_callee` and
`implementations_by_trait` with a *substring* filter (the tool then keeps exact identifier matches
unless `match: "substring"`). The keys are length-prefixed, so a prefix scan cannot serve them and the
naive shape walks every key. Two resident structures keep the common queries in the millisecond range
without changing a single result:

- **Name dictionary** (`src/index/name_dict.rs`). The distinct callee and trait names (orders of
  magnitude fewer than the keys; a few MB at monorepo scale) live in one contiguous allocation. A
  query is a `memmem` sweep over it, and only the matching names' key ranges are scanned; keys, order,
  totals, the `total_is_partial` cap and cursors are those of the full walk. The writer records a name
  before staging its key, so the dictionary is a superset and a stale name costs one empty range scan.
  The first query after the index opens still walks the partition while a background pass builds it,
  and a needle matching over 5 % of the distinct names goes back to the walk.
- **Term index** (`src/mcp/term_index.rs`). `code_symbols` and the `code_refs` importer lookup search
  only names and import
  strings, held per file with no spans or signatures; a hit fetches its span and signature from the
  outline cache by `(path, symbol index)`, so only the returned page is decoded. It is built in the
  background when a full cache is published and patched, not rebuilt, by an incremental rescan.

Both are memory only: nothing is persisted or migrated.

Because Fjall admits one process, `src/mcp/index_route.rs` picks where a reference read runs:
`Local` (this session holds the index: a writer or the CLI), `Host` (a daemon-hosted connection,
reaching the daemon's workspace pool in-process), `Daemon` (a session that cannot open the index
forwards an `IndexRead` request over the socket; the daemon runs the same scan functions via
`src/mcp/index_read.rs`, so results, totals and cursors equal a writer's and the session holds no
projection), or `InRam` (nothing reachable: a lazily built, byte-budgeted projection of the blobs that
can be truncated and says so with a `projections_capped` notice). A failed forward degrades to
`InRam` rather than erroring. The same request type carries the `code_grep` bloom lookup
(`IndexReadQuery::GrepBloom`, at most 16,384 paths per request).

### Grep prefilter

`code_grep` is a sweep over every indexed file that passes the language/path filters. A per-file
trigram bloom (`grep_bloom`: size/8 bytes clamped to 64 B–256 KiB, two hash positions per trigram,
about 12.8 % of the indexed code bytes) lets it skip files that cannot match: the pattern's
required literals (`regex-syntax`'s literal extractor, prefix and suffix sets, each at least 3 bytes)
are tested against the bloom, and a file is skipped only if its row rejects them AND a `stat` still
shows the `(size, mtime_ns)` the row was built under. The real regex still runs on every candidate, so
a false positive costs a read and never a wrong result; patterns with no required literal sweep
everything, and `BASEMIND_GREP_BLOOM=0` forces the full sweep. Nothing is resident. See ADR-0012 for
the sizing and measurements.

## Schema versioning

Two on-disk schemas track `RELEASE_MINOR` from `src/version.rs`, plus a finer-grained extractor epoch:

- `INDEX_SCHEMA_VER` in `src/index/mod.rs` — Fjall partition / key encoding: `RELEASE_MINOR` plus
  `INDEX_PARTITION_REVISION`, bumped when the keyspace layout changes between releases. A wholly new
  keyspace whose absence reads as "nothing known" (such as `grep_bloom`) needs no bump. The probe
  keyspaces are the variant that is not harmless empty, so they carry a `meta` stamp
  (`import_probe_ver`) instead: absent means "run the wholesale resolve pass once", with no wipe.
- `SCHEMA_VER` in `src/extract/mod.rs` — msgpack blob format
- `EXTRACT_EPOCH` in `src/extract/mod.rs` — extraction *output* revision; see below.

Bump cadence:

- Minor release (`0.1.x` → `0.2.0`) bumps `RELEASE_MINOR` → each view's index and blobs reset
  on next scan.
- Patch release (`0.1.0` → `0.1.1`) MUST be cache-compatible — never bump `RELEASE_MINOR` from
  a patch commit. A patch that changes what an unchanged file's blob should contain (not its
  serialized shape) bumps `EXTRACT_EPOCH` instead: every `FileEntry` and L1 blob records the epoch it
  was produced under, and the next scan re-extracts only files whose entry or blob predates it, inside
  the normal memory-bounded pass and keeping their call tier. The scan summary reports `refreshed`
  (and `tier_migrated` for tier moves).

Wipe-on-mismatch is the migration story for the schemas; the next `basemind scan` rebuilds from
source. What happens to an existing index on each kind of change, and the checklist for adding a new
on-disk component, is in [UPGRADING.md](UPGRADING.md).

## Blob store and GC

Extraction results live in a content-addressed blob store shared by every workspace on the machine
(`<cache>/cache/blobs/`); a workspace's `files` (code lane) and `doc_files` (document lane) entries
reference blobs by hash. The sweep (`src/store_gc.rs`, `src/store_gc_live.rs`) is tier-aware: a `.doc`
blob is live only if some workspace's `doc_files` references the hash, and `.fm` / `.chunk` / `.rref`
blobs only if a `files` entry does. That matters when a path changes tier (a markdown file that used
to be code-mapped keeps its content hash, so its dead code-lane blobs would otherwise be pinned by the
live `.doc` blob forever). The sweep is reference-counted against every workspace on the machine, keeps
blobs younger than 6 hours (`BASEMIND_BLOB_GC_GRACE_SECS`), and is serialised by `cache/gc.lock`. It
runs after any scan that migrated, refreshed or reset something (the daemon does so a minute later),
hourly in the daemon, and on `basemind cache gc`. There is no MCP tool for it: a session cannot sweep
without racing its own rescans. The size-budget enforcer and the last
sweep's health (`gc-state.json`, shown as `last_gc` by `cache stats`) are in `src/store_gc_budget.rs`.
Budget eviction never touches a locked, most-recently-used or recently built workspace (even over
budget, which is reported instead); the rules live in `src/store_gc_protect.rs`.

## MCP surface

The daemon hosts the MCP router (`rmcp`) over a per-workspace shared read stack; `basemind serve`
relays a client's stdio to it, and HTTP-native clients dial the daemon's streamable-HTTP front-end
directly (opt-in via `BASEMIND_ALLOW_HTTP`, bearer token in `<comms_dir>/http.addr`). The live
contract is `tests/mcp_smoke.rs`.

The surface is **one tool per job**, each with its own exact schema
([ADR-0018](adr/0018-mcp-verb-tools.md), which supersedes ADR-0011). There is no `mode` argument.
`src/mcp/types_tools.rs` is the single source of the tool names (`VERB_TOOLS`) and their arguments.

| Area | Tools | Gate |
|---|---|---|
| Code | `code_outline`, `code_symbols`, `code_read`, `code_grep`, `code_search`², `code_refs`, `find_files` | always |
| Documents | `docs_search` | always¹ |
| Git | `git_log`, `git_blame`, `git_diff` | always |
| Memory | `memory_save`, `memory_search` | always¹ |
| Index | `index_status`, `index_rescan` | always |
| Agents | `agents` (mode-dispatched: register, list, thread_start, thread_list, join, leave, members, add_member, remove_member, archive, post, history, message, inbox, ack, wait, cleanup, status) | `comms` |

¹ Advertised always, body-gated on the `memory` / `documents` features: a build without the feature
answers with an `isError` result naming it.
² Body-gated on `code-search` the same way.

`agents` keeps the ADR-0011 domain shape until its own redesign; `src/mcp/mode.rs`
holds its `mode` vocabulary and nothing else. Churn, changed-path regex search, chunk fetch, memory
deletion, cache maintenance and telemetry are CLI commands with no tool.

Contract of the 15 tools (`src/mcp/tool_contract.rs`, `src/mcp/strict.rs`):

- **Exact arguments.** Each tool has its own `deny_unknown_fields` struct with required fields
  required in the schema. Arguments are deserialized through `Strict<T>`, which never fails at the
  protocol layer: an unknown key is answered with a did-you-mean suggestion and the accepted list, not
  dropped. A stringified integer or boolean (`"limit":"10"`) is coerced; any other type error names
  the field. "Exactly one of" constraints cannot be stated in the flat schema subset hosts accept
  (`tests/mcp_schema_wire.rs` forbids `$ref` / `$defs` / `oneOf` / `anyOf` / `allOf`) and are checked
  in the tool body.
- **Positions.** `path` is a `RelPath` (byte-precise, repo-relative, forward slashes). `line`,
  `end_line` and `column` are 1-based in arguments and responses.
- **Lists.** Every list takes `limit` and `cursor` and returns `next_cursor`. A default `max_tokens`
  budget of 6000 applies; a trimmed list sets `budgeted`. Index scans use `scan_cap = limit * 8` to
  bound work on common names. The ranked tools (`code_search`, `docs_search`) page by offset over a
  larger top-k (`src/mcp/helpers_page.rs`).
- **Errors.** A failure the caller can fix is a tool result with `isError: true` whose text names the
  fix: a path that is not indexed says whether it is a document (use `docs_search`) or needs
  `index_rescan`; an empty index says to run `index_rescan` or `basemind scan`; a compiled-out
  feature names the Cargo feature. An empty index is never a plain empty result: responses carry a
  `notice` with `state: "empty_index"`.
- **Inline results.** Every call is answered inline. For a client that declared the Tasks extension,
  `index_rescan`, `code_search` and `docs_search` run on their own task and are handed to the task
  manager only when still running after 3 s (`TASK_THRESHOLD` in `src/mcp/tasks.rs`); until then the
  answer is an ordinary `tools/call` result.
- **Always loaded.** Every verb-tool definition carries `_meta["anthropic/alwaysLoad"]: true`, so a
  host that defers MCP tools loads the schemas at session start and no tool-search round trip is
  needed. `agents` is not marked and stays deferred
  ([ADR-0020](adr/0020-verb-tool-contract-amendments.md)).
- **Cursors.** A `next_cursor` is bound to the query that issued it (`src/mcp/cursor_bind.rs`): the
  tool name and the arguments that select the list, hashed. Passed to another query, or used after
  the index or HEAD moved, it is an `isError` result saying to call again without `cursor`.
- **Paths.** A caller-supplied path is read from the working tree only after it is found in the
  index and shown to stay inside the repository (`tool_contract::working_file`): an absolute path, a
  `..` component and a symlink pointing outside are refused.
- **Annotations.** Exact per tool: everything is read-only except `memory_save` and `index_rescan`,
  and nothing is destructive. No `output_schema`.
- **Size.** `tools/list` for the 15 tools is about 18 KB (about 4.5k tokens), down from about 25 KB
  (about 6.3k tokens) for the four domain tools they replace. `tests/mcp_schema_wire.rs` enforces a
  20,000-byte ceiling; a description that pushes past it is shortened, the ceiling is not raised.
- **Descriptions.** Each description says when to use the tool and carries one example call; field
  descriptions are kept to a clause because every session pays for them.
- **Telemetry.** The telemetry key of a call is the tool name.
- **CLI parity.** Every MCP tool has a CLI command, checked by `tests/cli_parity/`
  ([ADR-0014](adr/0014-cli-mcp-parity-guard.md), amended by ADR-0018); the CLI also has shorthands and
  administrative commands with no tool.
- **Layout.** Tool bodies live in `src/mcp/helpers*.rs`, area-sliced (see
  [Source layout](#source-layout)); `tools.rs` and the `tools_<area>.rs` siblings contain `#[tool]`
  shims only.

## Git layer

`gix`-backed log, blame, diff, and status. The git cache at
`<cache>/cache/workspaces/<workspace_key>/git-cache/` has two tiers:

- An in-process LRU (1024 entries per category).
- A sha-keyed disk store: `commit_files/<sha>.msgpack`,
  `log/<head_sha>__<scope>.msgpack`, `blame/<sha>__<path_hash>.msgpack`.

Commits are immutable, so once a sha-keyed entry is on disk it's valid forever.
HEAD-keyed entries (`log`) roll off naturally when HEAD moves.

Drop the disk cache with `basemind cache clear`.

### Git-history index

A separate, per-workspace Fjall store at `git-history.fjall/` in the workspace cache directory — distinct from the
`git-cache/` blame/log tier above. It turns `git_log` (recent commits, commits touching a path, the
commit walk behind `path` + `symbol`, and full-text `query` / `author` search) and the CLI's
`git churn` and `git by-path` from live history walks into posting-list lookups. Source:
`src/git_history/{mod,builder,reader,fts,keys,encoding,proto,remote}.rs`.

- It is repo-global (identical across the working/staged/rev views) and carries its own
  `GIT_HISTORY_SCHEMA`, independent of the code-map `INDEX_SCHEMA_VER` — a code-map schema bump
  must never throw away a 200k-commit walk.
- Fjall's directory lock is exclusive even for a read-only open, so exactly ONE process may hold
  this database. Which process that is depends on the deployment, modeled as a `Backend::Local
  | Backend::Remote` enum on `GitHistoryIndex`:
  - **Standalone** (no comms daemon): the process that opens it (`scan`, a writable `serve`, a
    one-shot CLI query) holds it locally and, if a writer, builds it in-process.
  - **Daemon-backed**: the daemon — the machine's sole fjall writer — holds and builds the
    database; every front-end (a `daemon_writer` serve, the one-shot CLI) holds a `Remote` proxy
    that forwards each history query over the socket rather than trying (and failing) to open it.
- A process that can reach neither backend falls back to the live walk (`git_history: None`). The
  index is a pure accelerator: tools use it only when `last_indexed_head == HEAD` and otherwise
  live-walk and report `partial: true`, so it can never serve stale results.
- A linked worktree shares the MAIN worktree's index (keyed on the main worktree root) rather than
  rebuilding its own, since the commit graph is identical across worktrees of one clone.
  The index tracks one head: a linked worktree whose HEAD descends from it appends the new commits,
  while one that diverges leaves the index untouched (`RebuildOutcome::Skipped`) and answers by live
  walk, so switching worktrees never wipes and rebuilds the history. The first build may come from any
  worktree. Index sync runs on a dedicated two-thread pool.

## Resource governance

The `[resources]` config table (`src/config/resources.rs`) is the single place an operator bounds
basemind's footprint on a constrained machine:

| Field | Default | Purpose |
|---|---|---|
| `scan_threads` | `0` (auto: rayon's per-core default) | Cap on the code-map scanner's rayon pool. |
| `embed_threads` | `0` (auto: `max(2, logical_cpus / 4)`) | Cap on the ONNX embedding pool. |
| `embed_batch_size` | `32` | Chunks submitted to ONNX per batch. |
| `max_concurrent_documents` | `0` (unbounded) | Cap on concurrent document extraction, enforced by a counting semaphore (`src/backpressure.rs`). Document extraction then passes `FootprintGate::admit_exclusive`: over `max_footprint_mb` only one document runs at a time, and a document whose header-derived estimate (`src/extract/doc_cost.rs`) exceeds the whole ceiling is skipped as too large. Distinct from `max_footprint_mb`, which reacts to memory already allocated: an extraction's spike lands faster than the 50 ms sampler sees it, so this bounds how many spikes may overlap before the first one starts. |
| `document_models` | `full` | Model families document extraction runs: `full` (every configured post-processor the build has; OCR needs `documents-ocr`, NER needs `documents-ner`) \| `code_only` (embeddings only, no keywords/NER/summarization/OCR) \| `none` (metadata + keyword search only). |
| `onnx_provider` | `cpu` | ONNX execution provider for embeddings, reranker and NER: `cpu` \| `auto` \| `coreml` \| `cuda` \| `tensorrt`. `auto` on macOS means CoreML, which compiles per input shape and measured 7.6 GB for one tiny document vs 0.8 GB on CPU. |
| `max_footprint_mb` | `0` (auto) | Ceiling on process memory footprint. A positive integer is an explicit mebibyte ceiling; `0` or `"auto"` derives one from the environment (75% of an enforced cgroup limit, else 25% of machine RAM, floored at 2048 MiB); `"off"` disables the gate. |
| `max_map_cache_mb` | `256` | Byte budget for the MCP read stack's decoded-outline cache (`src/mcp/l1_cache.rs`), per workspace. `0` = unbounded. A miss costs one blob read and never changes an answer. A read-only session's projected call / implementation indexes are charged against it. |

In the shared daemon these values are ceilings, not settings: `src/config/daemon.rs` applies
`min(file, cap)` to each (and to `[scan]` / `[documents]` limits), with `0` / `"auto"` /
`"off"` resolving to the cap, and the operator raises a cap through `BASEMIND_DAEMON_MAX_*` in the
daemon's environment. ONNX Runtime memory is bounded in every process (`embeddings::bound_ort_memory`:
no memory-pattern planning, no retained CPU arena; intra-op threads are the auto embed-thread count, or
2 in the daemon), and the daemon drops resident embedding engines when the last concurrent embedding
pass ends.

Other bounds that are not `[resources]` keys:

- **Stack-graph resolution** (`src/intel/stackgraph.rs`) abandons a file past 600,000 steps, 50,000
  partial paths or a 3 s budget (degrading to the `locals` fallback) and runs at most 2 builds at a
  time; a wall-clock budget alone let parallel workers stitching 100-200 KB files take a large-monorepo
  scan from ~2 GB to 9-13 GB.
- **Panicking embed jobs** are contained (`catch_unwind`) so a panic surfaces as an error instead of
  aborting the process through rayon.
- **Allocator.** `mimalloc` is the global allocator (it returns freed pages to the OS). On macOS
  `src/alloc_tag.rs` retags its mappings from Mach VM tag 100 (`VM_MEMORY_IOACCELERATOR`) to 254
  (application-specific), so `footprint`, `vmmap` and Activity Monitor report the heap as application
  memory, not "GPU" memory.

`[resources]` bounds **one** read stack. The daemon holds several, and that multiplier is its own
bound (`src/comms/workspace_pool.rs`). A hot pool entry is an open `Store` handle, capped at 4
(`DEFAULT_HOT_CAP`); a *warm read stack* is the O(corpus) structure `max_map_cache_mb` governs, and
it is capped separately and far lower:

| Knob | Default | Purpose |
|---|---|---|
| `BASEMIND_WARM_READ_STACKS` | `3` | Daemon-hosted read stacks kept warm at once. Building one past the cap sheds the least-recently-used, skipping any workspace with a live connection. |
| read-stack idle TTL | `10 min` | A read stack unrequested this long is dropped while its hot store entry survives (swept every 60 s), against the 15-minute TTL for the entry itself. Deliberately not shorter: the warm-stack **cap** is what bounds residency, so a tighter clock buys at most three stacks while reaching the staleness window below more often. |

Both are machine-global daemon knobs, so they are constants with an env override rather than
`[resources]` keys: the pool is built before any workspace is known, and a per-workspace config key
would have no answer to which workspace's number governs the shared daemon. Shedding a stack is safe
because it is a projection of the same on-disk blobs — the next connection rebuilds it and answers
identically; what it costs is latency. It does stop that workspace's filesystem watcher until the
rebuild, so edits made in the gap are picked up on the next watcher event or rescan rather than
immediately — the same window a daemon restart or the 15-minute entry sweep already opens.

The best-effort `FootprintGate` backpressure (`src/backpressure.rs`) samples the process
footprint via `src/sysres.rs` and parks the calling worker in a bounded backoff loop while the
process is over the ceiling. It is deliberately never a hard invariant: an unreadable sample admits
immediately, and a sustained overshoot admits anyway after `max_wait` (5s) to guarantee forward
progress — the gate shaves the scan's peak memory, it never fails a scan. It acts at three levels:

- **Drive loop** (`src/scanner_drive.rs`): candidates are processed in chunks and the gate is
  consulted between chunks, holding no index batch. Each admit that had to be waited out halves the
  chunk size and worker count, and consecutive clear admits widen them back, so a scan that cannot
  hold its ceiling converges on a narrower, slower pass instead of a dead one.
- **Leaves, advisory** (`admit`): a large-file parse waits, then proceeds after `max_wait`; there is no
  smaller unit of work to fall back to.
- **Document extraction, exclusive** (`admit_exclusive`): over the ceiling at most ONE document is
  admitted at a time (the admission holds a process-wide token), and an otherwise idle process is
  admitted at once, since waiting on an idle process can never free memory. Before extraction a
  document's peak memory is estimated from its header (`src/extract/doc_cost.rs`: raster pixels,
  OOXML/ODF uncompressed size, per-type factors, 32 MiB floor); one whose estimate exceeds the whole
  ceiling is skipped and counted as too large, with a warning naming the estimate and the limit.

`src/sysres.rs` (plus `src/sysres_cgroup.rs`) reports both usage and the *limit* the platform
imposes, which is what makes `auto` possible:

| Platform | Usage | Limit |
|---|---|---|
| Linux, cgroup v2 with an enforced limit | `memory.current` − `inactive_file` | `memory.max`, minimum over the hierarchy |
| Linux, cgroup v1 with an enforced limit | `memory.usage_in_bytes` − `total_inactive_file` | `memory.limit_in_bytes` (sentinel = none) |
| Linux, no enforced limit (no cgroup, no `memory.max`, or one above RAM) | `/proc/self/statm` resident pages | `MemTotal` from `/proc/meminfo`, marked *not enforced* |
| macOS | mach `TASK_VM_INFO` `phys_footprint` | `hw.memsize` |
| Windows | `K32GetProcessMemoryInfo` working set | `GlobalMemoryStatusEx` total physical |
| anything else | unavailable — the gate is a no-op | unavailable |

A reading reports whether its limit is *enforced* (a cgroup limit the kernel kills you for
crossing). Only an enforced limit is measured against the cgroup's working set. Without one — no
`memory.max`, the default for a plain `docker run`, or a limit larger than `MemTotal` — the cgroup
is often shared (a CI runner's service, a desktop session's scope), and its working set counts other
processes and active page cache that throttling this process can never release, so the reading
falls back to the process's own RSS against `MemTotal`.

Readings are cached for 50 ms so an admit point called per file from every rayon worker does not
turn a memory problem into a syscall storm. `auto` budgets **75%** of an enforced limit or **25%**
of an advisory one, floored at 2 GiB so a small container cannot produce a ceiling the ONNX
runtime alone would sit above.

**Not a memory bound, and not a bug:** `SCANNER_STACK_SIZE` (256 MiB, `src/scanner_file.rs`) is the
per-worker stack *reservation* for the scanner's rayon pool. A thread stack is lazily-committed
virtual address space — only the pages actually touched become resident — so it never counts toward
RSS or a cgroup limit, and it appears in no memory measurement this section describes. It looks
alarming in `ps` output under `VSZ` and is a recurring false lead in memory investigations. It is
sized for deeply nested syntax trees; shrinking it saves nothing and reintroduces stack-overflow
aborts.

## Hardening

`tests/harden.rs` is the real-OSS canary harness. `#[ignore]`-gated; run with:

```bash
cargo test --release --test harden -- --ignored --nocapture
```

It clones 8 upstream repos under `/tmp/basemind-harden/` (`ripgrep`, `tokio`,
`typescript`, `react`, `django`, `requests`, `gin`, plus a shallow `ripgrep`
variant), runs `basemind scan` on each, then sweeps the code tools plus
a representative subset of `git_log` / `git_blame` / `git_diff` calls. Canary assertions catch
regressions:

- **tokio**: `code_refs {name: "spawn"}` ≥ 50 · `find_files {query: "src"}` ≥ 100 ·
  `code_grep {pattern: "fn spawn"}` ≥ 20 · `code_refs {name: "Future", relation: "implementations"}` ≥ 20
- **django**: `code_refs {name: "get"}` ≥ 50 · `git_log {query: "fixed"}` ≥ 20 commits ·
  `git_log {path: "django/db/models/query.py"}` ≥ 10 commits · `git_log {author: …}` ≥ 1 hit
- **react**: `code_symbols {name: "useState"}` > 0
- **ripgrep-shallow**: `any_truncated == true` (shallow-clone signal surfaces)

Every threshold sits well below the `limit` the capture call passes (react captures at `limit: 20`,
django `code_refs` at `limit: 200`), so a canary can never be satisfied merely by hitting the cap —
and never fails because upstream churn moved a count.

Per-repo metrics land at `/tmp/basemind-harden-*.log`.

## Agent comms & split memory

The daemon owns a separate Fjall store for agent communication (singleton per user):

```mermaid
flowchart TB
  subgraph agents["Agents (same machine, multiple repos)"]
    A1["Agent A — repo X"]
    A2["Agent B — repo Y (same workspace)"]
    HK["SessionStart hook\n(boot-subscribe + inject)"]
  end
  subgraph serve["basemind serve (per session: stdio relay to the daemon-hosted MCP router)"]
    MT["MCP tools: memory + agents"]
    CC["CommsClient (proxy)"]
    PEER["rmcp Peer (push, best-effort)"]
  end
  subgraph daemon["basemind daemon (singleton, user-global)"]
    FE["Front-ends: UDS"]
    THREADS["Thread registry\n(scope: repo | path-glob | subject)"]
    BR["Broker: explicit join, fan-out, refcount"]
    CS["CommsStore (Fjall):\nthreads · members · messages_by_thread(front-matter)\n· message_body · subs · cursors · agents · registry"]
  end
  subgraph cache["Global cache\n~/.local/share/basemind"]
    MEM["memory_by_key (scope, visibility, owner)"]
    LAN["LanceDB memory (+agent_id +visibility)"]
  end
  A1 --> serve
  A2 -->|own serve| daemon
  HK -->|discover threads| daemon
  CC -->|len-prefixed msgpack / JSON-RPC over UDS| FE --> BR --> THREADS --> CS
  BR -. push .-> CC -. notify .-> PEER
  MT --> cache
```

### Why a separate daemon

The daemon is the sole Fjall writer (across all repos on the machine). This allows N `basemind serve`
sessions on the same repo to read concurrently without downgrade-to-read-only races. The daemon is
a singleton enforced by socket-bind-as-lock (a Unix domain socket whose successful bind IS the lock);
stale sockets are reclaimed probe-before-unlink. It auto-starts on first need and stops on explicit `comms stop`.

#### Comms directory layout and protocol generations

The comms directory is `comms/` under the per-user data directory (`~/.local/share/basemind` on
Linux, `~/Library/Application Support/basemind` on macOS, `%APPDATA%\basemind\data` on Windows), or
`BASEMIND_COMMS_DIR`. Everything a daemon owns lives in a protocol-generation subdirectory of it:

```text
comms/
  g1/                 protocol generation 1 (RENDEZVOUS_GENERATION in src/daemon_lock.rs)
    comms.sock        Unix socket; on Windows the pipe \\.\pipe\basemind-comms-<USER>-<hash of this dir>
    daemon.lock       single-owner flock, held for the daemon's lifetime
    daemon.pid        the holder's record
    store.fjall/      threads, messages, agent roster
    daemon.log  http.addr  last-fatal.json
  comms.sock ...      releases up to 0.33: the same files, unscoped
```

A binary only looks inside its own protocol generation, so two installed releases of different
generations each run their own daemon and never contact, stop or signal the other's. The protocol
generation is not the comms protocol version (`PROTO_VER`), the scan generation or an index base
generation; it only names where a daemon and its clients meet. A protocol-version mismatch inside one
generation is reported, not resolved by a new directory. `<comms_dir>` elsewhere in
this document means the generation directory. The generation is bumped only when a release must stay
invisible to binaries already released into the current one; a new generation starts with an empty
comms store, and the previous generation's daemon runs until its own sessions end. Generations are
retired by disuse: `basemind comms cleanup` (`src/comms/generation_cleanup.rs`) removes another
generation's directory, or the unscoped files of releases up to 0.33, when its `daemon.lock` is free
and its newest file is older than 30 days. It reads the lock and file ages only, never this binary's
own generation, and runs only when asked.

The machine registry of live daemons is scoped the same way: a daemon registers under
`<data_home>/daemons/g1/`, where a release up to 0.33, which reads only the files directly in
`daemons/`, does not see it. `comms stop --all` and `comms doctor` (`--probe` included) list and
address the daemons of their own generation only. The ceiling on live daemons (`MAX_LIVE_DAEMONS`)
counts every generation's, by reading their registry records and never by contacting them.

Three things are not per generation, and bound what coexistence gives:

- **The workspace stores.** The code index, memory notes, vectors and git history are keyed by
  workspace or repository group and admit one process. When daemons of two generations want the
  same workspace, the first holds it and the second serves no index, memory or git history for it
  until the first exits.
- **The plugin launcher of 0.33 and older.** It still kills every basemind process of another
  version started from the plugin cache, on each of its own launches. Coexistence is complete only
  once no such plugin is installed.
- **The HTTP front-end's default port.** With `BASEMIND_ALLOW_HTTP`, each daemon prefers
  `127.0.0.1:51786`; the second to start binds an OS-assigned port and records it in its own
  `http.addr`. An address set with `BASEMIND_HTTP_ADDR` is never substituted.

Inside one generation there is one daemon, and a client that finds a daemon from another build never
signals one that answers. A daemon of the same or a newer release is never asked to stop: it is
used, or, when it is newer and speaks another protocol, reported (`update this basemind`). An older
one is asked to stop and, if it refuses because it is serving other sessions, is used when the
protocol matches and reported otherwise; until it goes, clients of the newer build on another
protocol have no daemon, and a `basemind serve` relay, at session start as after a restart, holds
requests for ten seconds and then answers them with that error while it retries with a growing
back-off. A daemon that closed its socket gets the whole exit window to
leave. SIGTERM and SIGKILL are reserved for a process still there after it, silent to `Stop` and
`Ping` when asked once more, that the generation's `daemon.pid` names at that moment. `comms stop`
and `comms stop --all` reclaim a daemon by the same steps and the same function. The full table is
the module doc of `src/comms/singleton.rs`.

### Daemon lifecycle and resource containment

The daemon is spawned detached with `setsid(2)`, in its own comms dir (never an inherited cwd — a
daemon that inherits `/` discovers a workspace root of `/`). It is started with a scrubbed
environment: the spawning agent's `BASEMIND_AGENT_ID` / `BASEMIND_PARENT_AGENT_ID` /
`BASEMIND_THREAD_ID` and the shell's `PWD` / `OLDPWD` are removed, everything else (PATH, HOME, XDG,
proxy and model variables) is inherited.

**`setsid` does not contain the daemon, and no code in basemind can.** It changes the session and
the process group; on Linux the child stays in the **spawning process's cgroup**. A daemon
auto-spawned from an interactive shell therefore lives in that shell's scope, outside whatever
`MemoryMax` an operator configured on a basemind unit, and a process cannot move its own children
into a cgroup it does not control. There is no in-process fix. Anyone who needs an enforced memory
ceiling must make their own unit the thing that *starts* the daemon.

`BASEMIND_NO_AUTOSPAWN=1` is what makes that possible. Set it and basemind connects to a daemon that
is already running but never starts one — the auto-spawn is the only path by which a daemon can
appear outside the operator's unit. Two deliberate exceptions: `basemind comms start` ignores it (an
operator who typed a start command has stated the intent the variable withholds from implicit
callers), and an already-live daemon is still used, because connect-only means *do not start one*,
not *do not use one*. `basemind daemon ensure` honours it, like every other implicit caller.

A working unit — with the install steps, the environment.d wiring, and the commands that verify the
ceiling is real rather than merely configured — ships at `docs/systemd/basemind-comms.service`.

### Request correlation, timeouts and the daemon log

Every client request after `Hello` travels as a `Call { id, request }` and is answered by a
`Reply { id, response }` (since protocol version 4; the current version is 5; `ensure_daemon` asks
an older daemon to stop on its version check, and a skewed peer fails `Hello` with `proto_skew`). A client that abandons a request
mid-flight (a timeout, a cancelled tool call) therefore cannot mistake the late reply for the next
request's answer: stale ids are discarded, and only a drop in the middle of writing a frame forces
a reconnect. Bare requests (status probes, `Hello`) still get an uncorrelated `Response`.

Nothing waits forever. A coordination request must be answered within
`BASEMIND_COMMS_REQUEST_TIMEOUT_SECS` (default 10) and the connect + `Hello` within
`BASEMIND_COMMS_HANDSHAKE_TIMEOUT_SECS` (default 30: a cold daemon answers `Hello` only once its store is open), else the call fails with the retryable
`comms: broker unresponsive`. Forwarded work (scan, embed, memory, git-history, index reads) is
exempt. The stdio relay answers a request the daemon has not replied to within
`BASEMIND_RELAY_REQUEST_TIMEOUT_SECS` (default 180, `0` disables) with a retryable `-32002
backend_timeout` and drops the late reply. The MCP `wait` mode is capped at 40 s, returns early
when the host cancels the call or the connection closes (the ephemeral link, and so its
subscription, is dropped at once), and does not return messages a previous `wait` already returned.

The detached daemon's stdout and stderr are appended to `<comms_dir>/daemon.log` (mode 0600,
rotated to `daemon.log.1` past 8 MiB at spawn and trimmed in place while running). `basemind comms
doctor` prints the path for every comms daemon (`log` in `--json`).

### Daemon runtime, store access and early accept

The daemon is mostly an idle I/O relay, so it runs a small multi-thread tokio runtime of 8 async
workers (`DAEMON_ASYNC_WORKERS` in `src/cli/comms_daemon.rs`) and a capped blocking pool. The comms
store is Fjall, so every call is blocking disk I/O. Handlers never run it on a worker: reads go
through `Broker::store_blocking` and mutations through `Broker::store_write`
(`src/comms/daemon_store_blocking.rs`), both of which hand a closure to `spawn_blocking`. Writers
additionally take a plain-mutex write gate so read-modify-write sequences (a post's `seq`
allocation, thread-record updates) stay serialized exactly as when every handler ran inline. A slow
disk therefore cannot stall `Ping` or `Status` on another link, and a plain inbox read skips the
gate.

On Unix the singleton socket is accepted before the store has finished opening (`EarlyAccept` in
`src/comms/frontend_uds.rs`). Until the broker is published, only `Ping` is answered, so
`ensure_daemon` sees a healthy daemon. Every other request, and relay connections, park until the
broker exists and are then served as if they had arrived afterwards; nothing is dropped.

### Idempotent posts and relay replay

`agents` `post` accepts an `idempotency_key` (1-128 printable ASCII characters; the MCP layer
generates one when the caller omits it). The store records `(from, thread, key) -> message id` in the
`meta` keyspace in the same batch as the message, so rows survive a daemon restart. A repeat within
the one-hour window (`IDEMPOTENCY_TTL`) returns the original id and appends nothing; older rows are
pruned.

The stdio relay (`src/stdio_relay.rs`) uses this to survive a backend restart without closing the
host's pipes. It reconnects with a 250 ms to 5 s backoff and re-sends each in-flight request at most
once (5 s replay timeout), but only when re-sending is safe:

- the read methods `ping`, `tools/list`, `resources/list`, `resources/read`,
  `resources/templates/list`, `prompts/list`, `prompts/get` and `completion/complete`;
- every call to a read-only code or git tool;
- `agents` modes `list`, `thread_list`, `members`, `history`, `message`, `ack`, `wait` and
  `status`, and `inbox` unless it sets `mark_read`;
- `agents` `post`, which the relay stamps with a `relay-` key when the caller gave none.

Every other in-flight request is answered with a retryable `-32001 backend_restarted` rather than
guessed at, and a request the backend never answered within the relay deadline gets `-32002
backend_timeout`.

### Delivery probe

An ordinary MCP tool response can carry a short notice of unread agent messages. Each tool call may
run a probe (`take_delivery_notice` in `src/mcp/helpers_comms.rs`) over a dedicated broker
connection so it never contends with the session's interactive client. It is rate-limited to one per
2 s per session, has a 200 ms read budget and a 2 s connect budget, scans at most 200 inbox rows
(the broker bounds its own scan, so the cost does not grow with the backlog), and lists up to five
messages, each at most once per session. Failure and contention are silent, so mailbox delivery
cannot make another tool slow or unavailable.

### Evidence a killed process leaves behind

A daemon that *diagnoses* its own failure records it (`comms/store_health.rs` writes
`last-fatal.json` before releasing the singleton) and `comms doctor` reports it. A `SIGKILL` reaches
none of that, so the OOM case — the one issue #62 is about — used to leave nothing at all.

`scan_evidence.rs` covers it from the other direction. A full-tree scan writes
`<workspace_cache_dir>/scan-inflight.json` before it starts and removes it on `Drop`, so a surviving
file proves the process never ran its destructors; the record's `phase` says how far it had got and
`root` names what it was scanning. `basemind comms doctor` lists every such record whose pid is dead
(filesystem-only, no RPC, so it stays usable on a wedged machine) and `--clear-fatal` acknowledges
them alongside the fatal-store record. Alongside it, a periodic `rss_mb` / `ceiling_mb` line makes
the growth curve that led to the kill reconstructable from the log. The sampler reads the footprint
every 200 ms and each line also carries `window_peak_mb` (the maximum since the previous line) and
`process_peak_rss_mb`; a gibibyte jump above anything already logged writes a `spike=true` line at once,
so a transient shorter than the 5 s line interval is still visible.

### Thread model

Threads are registered in a central registry owned by the daemon. Each thread has explicit membership
(list of agents) and at least one of: a subject, a scope (repo remote or path-glob), or named members.
Agents join threads explicitly via `agents` mode `join` or mode `thread_start`; no auto-join. Group scopes
(path-glob, repo) help agents discover relevant threads, but joining is always explicit.

### Condensed two-tier messages

A message is a front-matter envelope (`id, thread, from, ts_micros, subject, reply_to, body_len, body_sha`)
stored in `messages_by_thread`, plus a separately-stored body in `message_body` keyed by message id.
`agents` modes `history` and `inbox` scan front-matter ONLY — never the body — to stay token-frugal;
the body is fetched on demand by mode `message {message_id}`. Poster supplies a required short `subject`
plus an optional long body.

### Split memory

Repo memory is namespaced by `(scope, visibility, owner)`: the `memory_by_key` Fjall key is
`(scope, vis_byte, owner, key)` and the LanceDB memory table has `agent_id` + `visibility` columns.
Group memory (`visibility=group`, default, `owner=""`) is shared across agents in a repo; individual
memory (`visibility=individual`, `owner=agent_id`) is private to one agent. Schema is guarded by
`MEMORY_SCHEMA_VER` (derives from `RELEASE_MINOR`); a mismatch wipes and rebuilds.

The Fjall half (`memory_by_key`) is stored once per repo group, not per
view: `<cache>/cache/repos/<group>/memory.fjall/`, where `<group>` is the blake3 of the canonical main
worktree root (the parent of the git common dir), so every linked worktree of a clone shares it; a
root outside git uses its own workspace key. The daemon stays the sole writer and holds one handle per
group. No garbage collector lists `repos/`: budget eviction and the orphan reaper only touch
`workspaces/<key>/`, so evicting a cold workspace never deletes memory. On open, rows an older build
left in a view's own `memory_by_key` keyspace are merged into the group store (last writer wins by
`updated_at`; ties keep the larger payload) and the view's keyspace is dropped, before any
schema-mismatch wipe. A seeded worktree view therefore carries no memory after its first open.

### Worktree registry

The daemon maintains a registry of the workspaces it has seen and each repo's worktrees and local
branches, populated from git plumbing as sessions connect. It is read-only to serving sessions and
listed by `basemind workspace workspaces` / `worktrees` / `branches`; it has no MCP tool and holds
no claims.
