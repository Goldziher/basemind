---
status: accepted
date: 2026-11-15
supersedes: "0019"
---

# ADR-0023: Library crate layering, revised

- **Deciders:** basemind maintainers
- **Related:** supersedes ADR-0019; ADR-0014, ADR-0024, ADR-0025, ADR-0026

## Context

ADR-0019 decided to split the single `basemind` package into four crates so that another
application can embed the engine in-process while the CLI and MCP server keep shipping. The goal
stands. An audit of the code on `main` found that the split as written cannot be carried out:

- **Placement.** ADR-0019 puts all of `extract` and `config` in a core that "must not depend on
  tokio, rmcp, clap". Document extraction builds a tokio runtime (`extraction_runtime` in
  `src/extract/doc.rs`), as does the vector store (`LanceStore::open` in `src/lance/mod.rs`). The
  CLI-override config derives clap types (`src/config/overrides.rs`). The file watcher takes tokio
  channels (`src/watcher.rs`). The daemon-backed git-history client uses `rmcp`, tokio and the
  comms client (`src/git_history/remote.rs`).
- **Reverse edges.** Modules meant for core import the layers above them:
  - `src/config/v1.rs` uses `crate::mcp::params::Visibility`;
  - `src/path.rs` implements `rmcp::schemars::JsonSchema` for `RelPath`;
  - `src/git_history/mod.rs` takes a `crate::comms::ids::AgentId`;
  - `src/store_gc_workspace.rs` imports `crate::comms::identity::AGENT_ID_FILE`;
  - the tests in `src/index/layered_view.rs` call `crate::mcp::scan_calls_by_name`;
  - `scanner.rs`, `scanner_drive.rs`, `scanner_file.rs`, `scanner_flush.rs`, `store.rs` and
    `store_blob.rs` name the document and vector stack directly, behind feature gates.
- **Git is not optional.** Scan and store call `crate::git` (`scanner.rs`, `scanner_file.rs`,
  `scanner_flush.rs`, `store_layout.rs`, `store_base.rs`, `store_seed.rs`), which wraps `gix`.
- **No embedding model.** ADR-0019 does not say whose cache an embedded engine uses. The cache root
  is a process-wide function (`cache_root` in `src/store_layout.rs`, read from `BASEMIND_DATA_HOME`
  or the platform data directory), and Fjall admits one process per directory, so an embedded
  engine and the machine daemon cannot both open one workspace's index.
- **Publishing.** `Cargo.toml` pins `xberg` by git revision. ADR-0019's publish order assumes all
  four crates can publish.
- **Shims.** ADR-0019 keeps deprecated re-exports of the old module paths for two minor releases.
  There is no external consumer to keep them for.

## Decision

Four crates, dependencies pointing downward only:

| Crate | Holds | Must not depend on |
|---|---|---|
| `basemind-core` | scan, blob store, code index, tree-sitter extraction, git (`gix`), config types, paths, and the traits through which scan hands chunks to a sink, an embedder and a vector store | tokio, rmcp, clap, xberg, lancedb |
| `basemind-retrieval` | document extraction (xberg), embedding, the vector store, chunk lanes | rmcp, clap |
| `basemind-engine` | the `Engine` / `Workspace` API; optional dependency on retrieval | rmcp, clap |
| `basemind` | CLI, MCP adapters, comms and the daemon, the file watcher, the daemon-backed git-history client, CLI-override config, the eval harness, the binary | |

1. **Reverse edges are cut first.** Each edge listed in Context is removed in place, in the single
   crate, before any file moves: `Visibility` moves to config, `RelPath` implements
   `schemars::JsonSchema` directly, git history takes an opaque caller id, the agent-id file name
   moves beside the store layout, the layered-view tests move to where the scan functions live, and
   scan reaches documents and vectors only through the traits.
2. **An embedding application owns its data.** It constructs the engine with its own data root and
   holds its own Fjall handles. It does not attach to the machine daemon and does not share the
   plugin's cache. Sharing a cache between an embedded engine and the daemon is out of scope until
   the engine can act as a daemon client. The data root becomes a constructor argument; no core
   code reads `BASEMIND_DATA_HOME`.
3. **Async stops at retrieval.** The engine exposes typed synchronous operations with a typed
   error. Core is synchronous and parallel through rayon. The only runtime the libraries own is the
   one `basemind-retrieval` builds internally for xberg and LanceDB; it does not appear in any
   public signature. The root crate owns the server's runtime.
4. **Publishing.** Only `basemind-core` can publish until `xberg` is a crates.io release. Retrieval
   publishes then, and engine and the root crate follow retrieval, because the engine's optional
   dependency on retrieval must resolve on crates.io.
5. **No deprecated re-export shims.** Old module paths break in the release that moves them.
6. **Sequence.** In-place layering comes before the format work of release 0.35: cutting the
   reverse edges, the trait seams, and an import-graph test that fails when a core module names
   `mcp`, `comms`, `cli`, `tokio`, `rmcp` or `clap`. The test is made to fail once on purpose before
   it is trusted. Extraction into separate crates comes after the worktree-index and retrieval
   rewrites, in release 0.36 (ADR-0026), so the crates are cut around the final formats.
7. **ADR-0014 restated.** The CLI and the MCP tools are both adapters over the same engine
   operation. Neither calls the other, and the parity tests compare two adapters, not a caller and
   its callee.

## Consequences

- **Duplicate index.** A repository used by both an embedding application and the plugin is scanned
  twice and stored twice, with separate blob stores and vector tables, and the two can disagree for
  as long as one is stale.
- An embedding application gets no file watcher, daemon or comms from the libraries. It rescans on
  its own schedule.
- Every embedder compiles `gix`, Fjall and the tree-sitter language pack. There is no git-free core.
- Until `xberg` is released, an embedder that wants documents or vector search must take
  `basemind-retrieval` and `basemind-engine` as git dependencies and cannot itself publish.
- Three releases of internal churn: layering in place first, then the formats, then extraction. The
  import-graph test is what keeps the layering from eroding in between.
- Code that imports `basemind::store` or any other internal module path breaks without a
  deprecation period.
- ADR-0019's remaining consequences still apply: the release-profile `opt-level` pins and the macOS
  link argument do not propagate to an embedder, so a build helper and a downstream-crate CI job
  are required.

## Alternatives considered

- **One crate with `cli` and `mcp` features, embedded with `default-features = false`** — the
  cheapest option, rejected for three reasons. The manifest would still carry the `xberg` git pin,
  so the crate could not publish at all, where a separate core can publish now. Features do not
  enforce direction: in a build with the released binary's features on, any module can import any
  other, so the reverse edges above would return unnoticed. And making `tokio`, `rmcp` and
  `clap` optional requires the same cuts (watcher, git-history client, CLI overrides, `RelPath`
  schema) as the crate split, without a compiler-enforced boundary to show for it.
- **Separate `basemind-mcp` and `basemind-comms` crates** — rejected: the MCP tools, the daemon and
  the comms client reference each other (the daemon hosts the tool router; the tools forward to the
  daemon), so the two crates would form a cycle or need a third to break it, and no consumer wants
  one without the other.
- **Retrieval inside core** — rejected as in ADR-0019: core would inherit the git-pinned dependency
  and two tokio runtimes, and could not publish.
- **An embedded engine as a daemon client now** — rejected: the daemon protocol is internal and is
  changing in the same releases (ADR-0021, ADR-0024). It is the route to a shared cache later.
- **Keep the deprecated re-exports** — rejected: they double the public surface for two releases to
  protect consumers that do not exist.
