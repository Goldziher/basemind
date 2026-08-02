# ADR-0001: Unified typed code-graph model

- **Status:** Proposed
- **Date:** 2026-08-02
- **Deciders:** basemind maintainers
- **Related:** ADR-0002 (edge provenance + confidence), ADR-0003 (graph traversal),
  ADR-0004 (community detection), ADR-0005 (rendering engine)

## Context

basemind knows the relationships between code elements — which symbol calls which, what a file
imports, what a type inherits or implements, and which uses it has *resolved* to a specific
definition. That knowledge is real and already indexed. What it lacks is a single, shared notion of
"the graph": today every capability that needs relationships assembles its own one-off walk for one
purpose (the architecture map builds a whole-repo graph, ranks it, and throws it away; the call
graph does a separate rooted walk) and then discards it. There is no typed, multi-edge graph object
that different capabilities can agree on.

We are about to add traversal (ADR-0003), community detection (ADR-0004), a rendering engine
(ADR-0005), and an interactive UI (ADR-0006/0007) — every one of which needs *the same* typed graph
over the same nodes. Building a third, fourth, and fifth bespoke walk would duplicate the traversal
logic and let the capabilities quietly disagree on what counts as an edge.

The constraints that bound the choice: basemind is offline, deterministic, and LLM-free; it holds
its index in a content-addressed store with an explicit schema version, so any *persisted* graph
would be new state carrying a version and a wipe-and-rebuild migration; and it is a hot-path scanner
where the cost of assembling relationships must stay bounded.

## Decision

Introduce **one shared code-graph model** that every graph-consuming capability reads:

- **Typed, multi-edge.** Distinct edge kinds — calls, imports, inherits/implements, resolved
  references (a proven use→definition binding), and containment (file→symbol / symbol→symbol
  nesting) — rather than a single undifferentiated "related to". Later ADRs add document and
  rationale edges (ADR-0008/0009) as further kinds on the same model.
- **Stable node identity.** A node is a code location (path plus its span), language-agnostic and
  deduplicated, so the same symbol is the same node across every capability and every edge kind.
- **Built on demand, in memory, not persisted.** The graph is assembled per query from relationships
  basemind already indexes, bounded by the same scan discipline the architecture map already honors,
  and discarded after. This keeps it a **read-side abstraction with no new persisted state and no
  schema bump.**

Every graph capability — the existing architecture map, the new traversal tools, the renderer, and
the UI — consumes this one model instead of re-deriving adjacency for itself.

## Consequences

- Traversal, communities, rendering, and the UI share one definition of "the graph" and one builder;
  a new edge kind or a new provenance rule (ADR-0002) is added once and every consumer benefits.
- The architecture map's currently-reserved import/inherit lanes become real edges on the shared
  model (delivered with ADR-0002), and the rooted call graph becomes one traversal over it
  (ADR-0003) rather than a separate walk.
- Cost stays proportional to the scan the architecture map already does; the shared builder must keep
  its bound and be measured against the performance baselines whenever it changes.
- Because nothing is persisted, there is no migration and no wipe. If per-query rebuild latency later
  becomes the bottleneck for interactive traversal or the UI, persisting adjacency is a *future* ADR
  — deliberately out of scope here.

## Alternatives considered

- **Persist a graph / adjacency store (or embed a graph database).** Rejected as premature: the
  per-call rebuild is already fast enough at scale and matches basemind's design, whereas a persisted
  graph adds a schema version, a wipe-on-mismatch migration, and write-path cost on every scan — none
  of which we need until traversal latency proves it.
- **Leave each capability with its own walk and just add three more.** Rejected: duplicates the
  traversal logic, invites edge-definition drift between the architecture map, the call graph,
  traversal, and the renderer, and multiplies the surface that must honor the scan bound.
- **Model the graph in the document / vector layer.** Rejected: that store is a RAG corpus with no
  byte-precise code node identity; the code graph belongs over the code relationships, and documents
  join it later as their own node kind (ADR-0008).
