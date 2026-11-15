---
status: accepted
date: 2026-11-15
supersedes: "0019"
amends: ["0014"]
---

# ADR-0023: Library crate layering, revised

- **Deciders:** basemind maintainers
- **Related:** supersedes ADR-0019; amends ADR-0014 ("the CLI runs the same tool code as MCP");
  ADR-0016, ADR-0024, ADR-0025, ADR-0026

## Context

ADR-0019 decided to split the single `basemind` package into four crates so that another
application can embed the engine in-process while the CLI and MCP server keep shipping. The goal
stands. An audit of the code on `main` found that the split as written cannot be carried out:

- **Placement.** ADR-0019 puts all of `extract` and `config` in a core that must not depend on
  tokio, rmcp, clap or jsonschema. Document extraction builds a tokio runtime
  (`extraction_runtime` in `src/extract/doc.rs`), as does the vector store (`LanceStore::open` in
  `src/lance/mod.rs`). The CLI-override config derives clap types (`src/config/overrides.rs`). The
  file watcher takes tokio channels (`src/watcher.rs`). The daemon-backed git-history client uses
  `rmcp`, tokio and the comms client (`src/git_history/remote.rs`).
- **Reverse edges.** Modules meant for core import the layers above them:
  - `src/config/v1.rs` uses `crate::mcp::params::Visibility`;
  - `src/path.rs` implements `rmcp::schemars::JsonSchema` for `RelPath`;
  - `src/git_history/mod.rs` takes a `crate::comms::ids::AgentId`;
  - `src/store_gc_workspace.rs`, and the tests in `src/store_gc_budget.rs` and
    `src/store_gc_tests.rs`, use `crate::comms::identity::AGENT_ID_FILE`;
  - the tests in `src/index/layered_view.rs` call `crate::mcp::scan_calls_by_name`;
  - `src/scanner_code.rs` imports `crate::lance` and `crate::embeddings`;
  - `scanner.rs`, `scanner_drive.rs`, `scanner_file.rs`, `scanner_flush.rs`, `store.rs` and
    `store_blob.rs` name the document and vector stack directly, behind feature gates.
- **Git is not optional.** Scan and store call `crate::git`, which wraps `gix`.
- **No embedding model.** ADR-0019 does not say whose cache an embedded engine uses. The cache root
  is a process-wide function (`cache_root` in `src/store_layout.rs`).
- **Publishing.** ADR-0019's publish order assumes all four crates can publish; `xberg` is a git
  dependency (ADR-0022, fact 2).
- **Shims.** ADR-0019 keeps deprecated re-exports for two minor releases, for no known consumer.

## Decision

Four crates, dependencies pointing downward only:

| Crate | Holds | Must not depend on |
|---|---|---|
| `basemind-core` | the core modules listed below, and the traits through which scan hands chunks to a sink, an embedder and a vector store | tokio, rmcp, clap, jsonschema, xberg, lancedb |
| `basemind-retrieval` | `extract/doc*.rs`, `scanner_docs.rs`, `embeddings.rs`, `lance/` | rmcp, clap |
| `basemind-engine` | the `Engine` / `Workspace` API; optional dependency on retrieval | rmcp, clap |
| `basemind` | `cli/`, `mcp/` (telemetry included), `comms/`, `watcher.rs`, `git_history/remote.rs`, `config/overrides.rs`, `eval/`, the binary | |

**Core modules:** `scanner*.rs` except `scanner_docs.rs`; `store*.rs`; `index/`; `extract/` except
`doc*.rs`; `chunk.rs`, `chunk_drive.rs`; `search/` (BM25, exact, fusion); `intel/`; `git/`,
`git_cache.rs`; `git_history/` except `remote.rs`; `registry/`; `query.rs`; `path.rs`; `lang.rs`;
`hashing.rs`; `version.rs`; `config/` except `overrides.rs`. Chunk blobs and the `intel` model
types are core because `store_blob.rs` reads and writes them. `chunk_drive.rs` and `search/` name
nothing forbidden today and need no cut.

1. **Reverse edges are cut first,** in place, before any file moves: `Visibility` moves to config;
   `RelPath` implements `schemars::JsonSchema` directly; git history takes an opaque caller id; the
   agent-id file name moves beside the store layout; the layered-view tests move to where the scan
   functions live; `scanner_code.rs` and the scan and store files reach `lance`, `embeddings` and
   document extraction only through the traits.
2. **An embedding application owns its data root.** It constructs the engine with a data root of
   its own and does not attach to the machine daemon or share the plugin's cache. No core code
   reads `BASEMIND_DATA_HOME`.
   - One process per data root, enforced by the workspace lock and Fjall's: a second process gets
     a lock error. Within its root the engine is the one writer ADR-0024 rule 5 requires.
   - The embedder triggers garbage collection through an engine call; nothing runs it otherwise.
   - Embedding model files are cached under the data root. Today `src/embeddings.rs` passes
     `cache_dir: None` and leaves the location to `xberg`.
   - Memory notes and git history are available through the engine.
3. **Async stops at retrieval.** The engine exposes typed synchronous operations with a typed
   error. Core is synchronous and parallel through rayon. The only runtime the libraries own is the
   one `basemind-retrieval` builds internally; it appears in no public signature.
4. **Publishing.** The order is the vendored forks (`crates/stack-graphs`, `crates/tree-sitter-graph`
   and `crates/tree-sitter-stack-graphs`, path dependencies today), then core, retrieval, engine
   and `basemind`. Retrieval, and so engine and the root crate, wait for an `xberg` release on
   crates.io if 1.4.0 is not there.
5. **No deprecated re-export shims.** Old module paths break in the release that moves them.
6. **Sequence.** In-place layering lands early in release 0.35, before that release's format work:
   the cuts of rule 1, the trait seams, and an import-graph test that fails when a core module
   names `crate::mcp`, `crate::comms`, `crate::cli`, `crate::lance`, `crate::embeddings`, `tokio`,
   `rmcp`, `clap`, `jsonschema`, `xberg` or `lancedb`. The test is made to fail once on purpose
   before it is trusted. Extraction into crates is release 0.36 (ADR-0026), around the final
   formats.
7. **Amendment of ADR-0014.** Its "the CLI runs the same tool code as MCP" becomes: the CLI and the
   MCP tools are both adapters over the same engine operation. Neither calls the other, and the
   parity tests compare two adapters, not a caller and its callee.

## Consequences

- **Duplicate index.** A repository used by both an embedding application and the plugin is scanned
  and stored twice, and the two can disagree for as long as one is stale.
- An embedding application gets no file watcher, daemon or comms. It rescans on its own schedule.
- Every embedder compiles `gix`, Fjall and the tree-sitter language pack. There is no git-free core.
- Until retrieval can publish, an embedder that wants documents or vector search takes
  `basemind-retrieval` and `basemind-engine` as git dependencies and cannot itself publish.
- The release-profile `opt-level` pins in `Cargo.toml` and the macOS link argument in `build.rs` do
  not propagate to a crate that embeds the engine. A build helper is required, and a
  downstream-crate CI job that builds and runs scan, outline and references on macOS and Linux.
- Moving the read-side logic out of `src/mcp/` is a large mechanical change. It lands as scripted
  moves, with the compiler and the import-graph test as the gate.
- Two releases of internal churn: layering in place and the formats in 0.35, extraction in 0.36.
- Code that imports `basemind::store` or any other internal module path breaks without a
  deprecation period.

## Alternatives considered

- **One crate with `cli` and `mcp` features, embedded with `default-features = false`** — the
  cheapest option, rejected. Features do not enforce direction: with the released binary's features
  on, any module can import any other, so the reverse edges would return unnoticed. Making `tokio`,
  `rmcp` and `clap` optional needs the same cuts as the split, without a compiler-enforced boundary.
  And the one manifest would carry the `xberg` git dependency for every consumer.
- **Separate `basemind-mcp` and `basemind-comms` crates** — rejected: the daemon hosts the tool
  router and the tools forward to the daemon, so the two would form a cycle, and no consumer wants
  one without the other.
- **Retrieval inside core** — rejected as in ADR-0019: core would inherit the git dependency and
  two tokio runtimes.
- **An embedded engine as a daemon client now** — rejected: the daemon protocol is internal and is
  changing in the same releases (ADR-0021, ADR-0024). It is the route to a shared cache later.
- **Keep the deprecated re-exports** — rejected: they double the public surface for two releases to
  protect consumers that do not exist.
