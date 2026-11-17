---
status: accepted
date: 2026-11-17
amends: ["0023"]
---

# ADR-0032: Library crate layering as built — module table, schema validation, open edges

- **Deciders:** the repository owner
- **Related:** amends ADR-0023; ADR-0029

## Context

ADR-0023 rule 6 asked for the reverse edges to be cut in place and for an import-graph test
(`tests/layering.rs`) that fails when a module names a higher layer or a forbidden crate. That work
landed, and doing it showed where ADR-0023's text and the code disagree:

- ADR-0023 puts every `scanner*.rs` except `scanner_docs.rs` in core. The vector-store and
  document-lane implementations of the scan's seams had to live in files of that name
  (`scanner_vectors.rs`, `scanner_docs_lane.rs`), and they name LanceDB, the embedding presets and
  document extraction, which are retrieval.
- Something has to choose which seam implementations a scan gets. ADR-0023 gives that choice to
  the engine but lists no engine module.
- ADR-0023 puts all of `config/` except `overrides.rs` in core and forbids `jsonschema` in core.
  `config/validate.rs` validates every config file against the bundled JSON Schema with
  `jsonschema`, and `config::load` and `parse_v1`, which call it, are core entry points.
- Sixteen modules exist that ADR-0023 does not place: thirteen core modules, `render`, `testing`
  and the engine module above.
- Three edges could not be cut on this branch without a behaviour change or ahead of other work.

## Decision

ADR-0023 stands except where this record changes it.

1. **The module table is `LAYERS` in `tests/layering.rs`.** A module carries its submodules unless
   a deeper entry overrides it. As built:

   | Crate | Modules |
   |---|---|
   | core | the ADR-0023 core list, except `scanner_docs_lane` and `scanner_vectors`; also `alloc_tag`, `backpressure`, `boot_identity`, `daemon_lock`, `disk_space`, `lang_rules`, `process_start`, `scan_evidence`, `scan_evidence_hosts`, `scan_phase_peaks`, `scan_retrieval`, `sysres`, `textmatch`; the crate root `lib.rs` |
   | retrieval | `embeddings`, `lance`, `extract::doc` (its `doc_blobs.rs` child included), `extract::doc_cost`, `extract::doc_fallback`, `extract::doc_guard`, `extract::doc_tiers`, `scanner_docs`, `scanner_docs_lane`, `scanner_vectors` |
   | engine | `retrieval_wiring` |
   | `basemind` | `cli`, `comms`, `mcp`, `eval`, `watcher`, `render`, `testing`, `config::overrides`, `git_history::remote` |

   - `scan_retrieval` holds the seam traits (`Embedder`, `VectorStore`, `DocumentSink`) and is core.
     `scanner_vectors` and `scanner_docs_lane` implement them and are retrieval. They keep their
     `scanner_` names until extraction moves them into the retrieval crate.
   - `retrieval_wiring` binds the seams to this build's implementations. It is the first engine
     module.
   - The crate root counts as core: a `pub use` in `lib.rs` is an edge like any other module's.
   - A new module is added to `LAYERS` in the commit that creates it. The test fails on a module
     with no entry.

2. **Schema validation stays in core until extraction.** `config::validate` may name `jsonschema`.
   It is the one exception to ADR-0023's forbidden-crate list, and only under these conditions:
   - `jsonschema` is taken with `default-features = false`. Every `$ref` in the bundled schema is
     internal and no `idn-*` format is used, so the HTTP and file resolvers, TLS and `idna` add
     nothing. Without them, `jsonschema` brings no reqwest, rustls, tokio or hyper into core.
   - No other core module names `jsonschema`.
   - At extraction, validation moves to the engine. Core's loaders then take the validator, or
     return the unvalidated value for the engine to check.

   An injected validator now would change `config::load`'s signature and leave a core-only caller
   unvalidated, for a crate boundary that does not exist yet.
3. **Known violations.** `KNOWN_VIOLATIONS` lists the edges that are still open. The test passes
   only on an exact match. When each edge disappears:

   | Edge | Disappears when |
   |---|---|
   | `src/config/validate.rs -> extern jsonschema` | validation moves to the engine at extraction (rule 2) |
   | `src/scanner_filter.rs -> retrieval_wiring` | the engine API exists and passes the `Retrieval` binding into the scan, so the scan stops asking for it |
   | `src/store.rs -> lance` | ADR-0029's retrieval store replaces the per-view `Store::lance` handle |

   - The `store.rs` edge is left on purpose. ADR-0029 rule 2 moves the LanceDB directory to the
     repository group, so this handle goes away with that work.
   - Until then, `VectorStore::{open, close, open_store_id, store_id_at}` exist only to manage the
     handle through `Store`. They change, along with their callers in `scanner_flush.rs`, when the
     handle moves.
   - Extraction (ADR-0023 rule 6) cannot start while any of the three edges remains.
4. **The test follows re-exports and exported macros.**
   - A path continues through `pub use` items, named or glob, and every module on the chain is an
     edge.
   - `crate::name!` and a bare `name!` of a `#[macro_export]` macro are edges to the module that
     defines the macro.
   - A `cfg_attr(test, ..)` argument and a `#[cfg(test)]` trait or impl method count as test code.
   - Each of these shapes is proved by the test's fixture crate.
5. **Public API moves.** These land without shims (ADR-0023 rule 5):
   - `extract::doc_guard::{DocPhase, PhaseSpent}` moved to `backpressure`.
   - `Store::write_doc` and `Store::read_doc_by_hex` are now `extract::doc::DocBlobs` methods. Under
     them sit the generic `Store::write_doc_blob` (public) and `Store::read_doc_blob_by_hex`
     (crate-private).
   - `GitHistoryIndex::remote` and `GitHistoryIndex::hosted` moved to
     `git_history::remote::{remote_index, hosted_index}`. `RemoteHistory::{indexed_head, commits}`
     are now `HistoryForwarder` methods. The trait also requires `hosts_in_process`, so a handle
     whose holder is this process never syncs over its own socket. `GitHistoryIndex::forwarder` is
     crate-private.
   - `config::ConfigLayers` is now `ConfigLayers<O>`, and `config::merge_layers` is generic. A
     caller with no override layer names `NoOverrides`.
   - `config::{DocumentsCliOverrides, load_with_overrides}` moved to `config::overrides`.

   Two re-exports remain: `comms::identity::AGENT_ID_FILE` and `mcp::types_memory::Visibility`
   (and so `mcp::params::Visibility`). Each points down from the root crate to core, so neither is a
   layering edge. Both are removed at extraction.

## Consequences

- The test and this record give the same answer for every module. A file that moves between
  layers changes `LAYERS` and this record's table together.
- Core still compiles `jsonschema` and its regex and number-format dependencies, but no network
  stack. An embedder that wants no schema validation cannot opt out before extraction.
- The three open edges set what has to happen before extraction: the engine API, ADR-0029's
  store, and the validation move.
- A re-export can no longer hide an edge. A `pub use` of a higher-layer item in `lib.rs` or in a
  lower-layer module fails the test, and so does every caller that goes through it.

## Alternatives considered

- **Rename the two retrieval files out of the `scanner*` glob now** (for example `lance/scan_rows.rs`
  and `extract/doc_lane.rs`). Rejected: extraction moves them into the retrieval crate anyway, so
  renaming now would rewrite the same files twice. The table above settles the conflict with the
  glob.
- **Inject the schema validator from the root crate now.** Rejected for the reasons in rule 2.
- **Replace `jsonschema` with hand-written checks.** Rejected: this changes which inputs are
  rejected and the error text, and duplicates a schema that is generated from the Rust types.
- **Move the LanceDB handle out of `Store` on this branch.** Rejected: ADR-0029 replaces the handle
  and its directory layout, so the same work would be done twice.
