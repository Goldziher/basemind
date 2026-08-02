# ADR-0001: Unified typed code-graph model

- **Status:** Proposed
- **Date:** 2026-08-02
- **Deciders:** basemind maintainers
- **Related:** ADR-0002 (edge provenance + confidence), ADR-0003 (graph traversal tools),
  ADR-0004 (community detection), ADR-0005 (rendering engine)

## Context

basemind's relationship data is real but fragmented across per-relation Fjall keyspaces and is only
ever assembled into a graph on-demand, per-tool, for one purpose:

- `architecture_map` builds an in-memory `RepoGraph` from scratch on every call
  (`src/mcp/helpers_archmap.rs`, `RepoGraph::build`), runs PageRank + Tarjan SCC over it, then
  discards it. Its edges are **single-typed** — `ArchEdge.kind` is always `"calls"`; the
  `imports`/`inherits` lanes are declared but not emitted (`src/mcp/types_archmap.rs:36`).
- `call_graph` does its own rooted, bounded BFS over call sites (`src/mcp/helpers_graph.rs`), a
  separate walk with no shared graph object.
- The underlying edges live in distinct keyspaces: `calls_by_callee`/`calls_by_path` (name-level
  call edges), `imports_by_module`/`imports_by_path` (import edges), `implementations_by_trait`/
  `implementations_by_path` (inheritance/impl edges), and `refs_by_def`/`refs_by_path` (**resolved**
  use→def edges — intra-file for all languages, cross-file for JS/TS via oxc). Node identity is
  consistently expressible as `(rel_path, start_byte)` with byte/row/col spans on every record.

We are about to add graph traversal (ADR-0003), modularity communities (ADR-0004), and a rendering
engine (ADR-0005) — all of which need a *typed, multi-edge* graph over the same nodes. Building a
third, fourth, and fifth bespoke walk (each re-deriving adjacency from raw keyspaces) would
duplicate the hot-path scan logic and let the tools drift apart on what counts as an edge.

Constraints: the module-size cap (1000 lines/file), the perf discipline (ahash/memchr, no clones in
inner loops, rayon not tokio on scan paths), and schema-and-blob compat (a persisted graph would be
a new keyspace with a version bump and wipe-on-mismatch).

## Decision

Introduce one shared **`codegraph`** module that builds a typed, in-memory, multi-edge graph over
the existing keyspaces, and make `architecture_map`, the traversal tools (ADR-0003), and the
renderer (ADR-0005) all consume it.

- **Node identity:** `(RelPath, start_byte)`, carrying the symbol name/kind/signature/span from the
  L1 outline where available; file-level nodes for imports.
- **Edge kinds (typed):** `calls | imports | inherits | uses | contains | resolves`, each edge
  tagged with provenance/confidence per ADR-0002. `contains` is the file→symbol / symbol→symbol
  nesting edge; `resolves` is the proven use→def edge from `refs_by_def`.
- **Construction:** extend/refactor the existing `RepoGraph::build` into `codegraph` rather than
  inventing a new loader — same prefix-scan-plus-L1-cache approach, same `ARCHMAP_EDGE_SCAN_CAP`
  bound, same ahash/memchr discipline. Callers request only the edge kinds and scope they need.
- **Lifetime:** the graph stays **built-on-demand and in-memory** (rebuilt per query, bounded), not
  persisted. No new Fjall keyspace, **no index-schema bump** — this is a read-side abstraction.

## Consequences

- Traversal, communities, and rendering share one definition of "the graph" and one hot-path
  builder; a new edge kind or provenance rule is added once and every consumer benefits.
- `architecture_map`'s reserved `imports`/`inherits` lanes become real (delivered together with
  ADR-0002), and `call_graph` can be re-expressed as a traversal over `codegraph` (ADR-0003) instead
  of a bespoke BFS.
- Cost stays proportional to the scan already done by `architecture_map`; the shared builder must
  keep its scan-cap and be measured against the harden baselines when it changes.
- Because nothing is persisted, there is no migration and no wipe. If per-query rebuild latency
  later becomes the bottleneck for interactive traversal or the UI, a persisted adjacency keyspace
  is a *future* ADR — deliberately out of scope here.

## Alternatives considered

- **Persist a graph / adjacency keyspace (or embed a graph DB).** Rejected as premature: the
  per-call rebuild is already fast enough for `architecture_map` at scale, it matches basemind's
  current design, and a persisted graph adds a schema version, a wipe-on-mismatch migration, and
  write-path cost on every scan — none of which we need until traversal latency proves it.
- **Leave each tool with its own walk and just add two more.** Rejected: duplicates hot-path scan
  logic, invites edge-definition drift between `architecture_map`, `call_graph`, traversal, and the
  renderer, and multiplies the surface that has to honor the scan-cap and perf rules.
- **Model the graph in the document/LanceDB layer.** Rejected: that store is a vector RAG corpus
  with no byte-precise code node identity; the code graph belongs over the Fjall code keyspaces.
