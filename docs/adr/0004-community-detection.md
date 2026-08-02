# ADR-0004: Community detection and deterministic labels

- **Status:** Proposed
- **Date:** 2026-08-02
- **Deciders:** basemind maintainers
- **Related:** ADR-0001 (unified typed code-graph), ADR-0005 (rendering engine),
  ADR-0006 (interactive UI)

## Context

A code graph becomes navigable when it is clustered into communities — groups of elements that
relate to each other far more than to the rest of the repo, i.e. the de-facto modules. The
architecture map already finds strongly-connected cycles, but a cycle is not a module: cycles catch
mutual recursion and dependency loops, not the broader community structure that drives a legend, a
show/hide filter, and — crucially — a meta-graph fallback when a full graph is too large to draw.
Knowledge-graph tools in this space ship community detection; it is also what keeps the UI (ADR-0006)
usable on a large repository.

The binding constraints: basemind is deterministic (the same repo must yield the same communities and
labels every run — required for a stable UI and for snapshot tests), LLM-free, and hot-path bounded.

## Decision

Add **modularity community detection** to the graph, alongside the existing cycle clusters:

- **Default algorithm: label propagation** — near-linear and hot-path friendly, made **deterministic**
  by pinning the seed and iterating in a fixed, sorted order (basemind's hashing is randomized, so
  iteration order must be fixed explicitly). "Deterministic" here means *reproducible for a given graph
  state* — the same repo yields the same communities every run — not stability across small edits, and
  label propagation can still produce lower-quality clusters on some graphs (which is what the Louvain
  opt-in is for).
- **Opt-in higher-quality algorithm (Louvain)** for callers who want cluster quality over speed.
- **Deterministic, LLM-free labels** — each community is named from its dominant path prefix and its
  most central member, so labels are reproducible without a model.

Communities become a per-node attribute on the shared graph, consumed by the renderer (legend and
styling), traversal (scoping), and the UI (filter + meta-graph fallback for large graphs). Detection
respects the same scan bound as the rest of the graph, with a timing guard in the performance
harness.

## Consequences

- Every node carries a community id and a human-readable label; the renderer and UI gain grouping and
  a scalable meta-graph view when a repo is too large to draw node-for-node.
- Determinism holds across runs — the same community assignments and labels every time — which is what
  makes the UI stable and the outputs snapshot-testable.
- One more bounded pass over the graph; guarded against the performance baselines.

## Alternatives considered

- **Expose the existing cycle clusters as "communities".** Rejected: wrong semantics — cycles are not
  modules; they miss the acyclic community structure that a legend and meta-graph need.
- **Make Louvain the default.** Rejected: better cluster quality but worse worst-case latency and
  harder to make deterministic; keep it as the opt-in quality setting.
- **LLM-generated community labels.** Rejected: non-deterministic and requires a model, violating the
  LLM-free and reproducibility constraints; deterministic path/centrality labels are good enough.
