# ADR-0003: Graph traversal capabilities

- **Status:** Accepted
- **Date:** 2026-08-02
- **Deciders:** basemind maintainers
- **Related:** ADR-0001 (unified typed code-graph), ADR-0002 (edge provenance + confidence),
  ADR-0005 (rendering engine), ADR-0006 (interactive UI)

## Context

With a unified typed graph (ADR-0001) and per-edge provenance (ADR-0002), basemind can answer
relationship questions it cannot answer today: *how does A reach B*, *what surrounds A within N
hops*, *what is the relevant neighborhood of this concept*. Right now the only graph queries are a
rooted call walk and a whole-repo architecture map — there is no path query, no filtered
neighborhood, and no scoped subgraph. An agent that wants these must stitch many primitive lookups
together, losing weighting and deduplication in the process.

These three shapes are table stakes for a knowledge-graph tool, and they are also the query surface
that the rendering engine (ADR-0005) and the UI (ADR-0006) render. We want them as first-class,
bounded, deterministic capabilities over the shared graph.

## Decision

Add **three traversal capabilities** over the unified graph, each exposed as an MCP tool:

- **path** — the shortest path between two symbols across mixed edge
  kinds, **confidence-weighted** so proven edges (ADR-0002) are preferred over inferred ones. By
  default `path` traverses *relationship* edges (calls, imports, inherits, resolved references) and
  **excludes containment/nesting**, which would otherwise yield structurally valid but semantically
  meaningless routes (A → its parent module → B); the caller can opt containment back in.
- **neighbors** — N-hop expansion outward from a symbol, filterable by edge kind, direction, and a
  minimum confidence floor.
- **subgraph** (explain) — the scoped neighborhood around a symbol or a query, cut to the significant
  head by graph centrality so the result is a readable subgraph rather than a dump.

Algorithm choice follows the question: an unweighted breadth-first walk (bidirectional for the path
query) when edge weights don't matter, and a confidence-weighted shortest-path search when they do.
Everything is **bounded** — node caps (`max_nodes`), a hop-depth cap, and a traversal scan cap
(plus a Dijkstra relaxation cap for `path`) — following basemind's existing tool conventions, and
everything is **deterministic and LLM-free**. Over-cap results set a `truncated` flag rather than
paginating.

Shipped scope is the single shortest path (not k-shortest), the three tools as MCP surface, and the
bounds above. Re-expressing the existing rooted `call_graph` over the shared model is deferred (see
Consequences); k-shortest paths and cursor pagination are out of scope unless real demand appears.

## Consequences

- Agents get whole relationship questions answered in one call instead of stitching lookups by hand,
  with weighting and deduplication done server-side.
- Rendering (ADR-0005) and the UI (ADR-0006) get their query surface: a path to highlight, a
  neighborhood to expand, a subgraph to draw.
- Traversal cost is bounded by the same scan discipline as the architecture map and is measured
  against the performance baselines; hub nodes cannot trigger unbounded work.
- The three tools (`neighbors`, `path`, `subgraph`) ship over the shared model. Re-expressing the
  existing `call_graph` on the same traversal is **deferred**: its name-keyed BFS is covered by live
  harden canaries, so replacing it is tracked as a separate, independently-verified change rather
  than folded into the tool-addition commit.

## Alternatives considered

- **Keep composing primitive lookups on the client.** Rejected: many round trips, no confidence
  weighting, no deduplication, and every agent reinvents the traversal — exactly the drift ADR-0001
  exists to prevent.
- **Ship unbounded traversal.** Rejected: a query rooted at a hub symbol would explode; basemind's
  contract is bounded, paginated, token-budgeted tools.
- **Introduce a graph query language / Cypher-like DSL.** Rejected as over-engineering for three
  well-understood shapes; revisit only if real demand for arbitrary queries appears.
