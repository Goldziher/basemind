# ADR-0009: Rationale / decision nodes

- **Status:** Proposed
- **Date:** 2026-08-02
- **Deciders:** basemind maintainers
- **Related:** ADR-0001 (unified typed code-graph), ADR-0002 (edge provenance + confidence),
  ADR-0008 (documents ↔ code graph)

## Context

The *why* behind code lives in places a purely structural graph can't see: inline rationale comments
(WHY / NOTE / TODO / FIXME / HACK) and references to decision records (ADRs, RFCs). Surfacing that
rationale as first-class, linkable nodes lets you traverse from a function to the decision that shaped
it — and it lets basemind's own decision records (this very `docs/adr/` tree) become part of the
graph. This is the second of the two stretch goals.

Unlike edge provenance (ADR-0002), which is derived on read from state basemind already computes,
rationale must be *extracted and classified* from free-form comment text — work too costly to redo on
every query. That makes it new persisted state, which carries a schema version bump and the standard
wipe-and-rescan migration.

## Decision

**Extract and classify rationale markers** from comments and decision-record citations, **promote them
to first-class rationale nodes** on the unified graph (ADR-0001), and **link each** to the code it
annotates (by proximity to the containing symbol) and to any decision record it cites. basemind's own
`docs/adr/` records are ingested the same way, so the decision graph is self-hosting.

This introduces a **new node kind with persisted extraction** in the code index, and therefore
**requires a code-index schema version bump** and its wipe-and-rescan migration — called out
explicitly, as the first ADR in this series to bump the code-index schema (ADR-0008 also persists, but
in the document store, not the code index), and noted in the changelog per project policy. Classification of free-form
comments is heuristic, so rationale links carry a confidence tag (ADR-0002).

## Consequences

- The graph answers "why is this here / what decision governs this": rationale becomes navigable and
  renderable (ADR-0005) alongside structure, and ADRs become queryable.
- Because the extraction is persisted, a schema bump + wipe is required — acceptable, bound to a minor
  release and announced in the changelog, but a real migration users pay once.
- Rationale classification is heuristic; confidence tagging keeps weak classifications visibly weak.

## Alternatives considered

- **Keep rationale as plain comment text with no node identity.** Rejected: that is the status quo —
  not traversable, not linkable, invisible to the graph.
- **Derive rationale on read without persisting.** Rejected: unlike the resolution signals in
  ADR-0002, extraction + classification of free-form comments is too costly to redo per query.
- **Model-based classification of comment intent.** Rejected for now: non-deterministic and requires a
  model; start with deterministic marker patterns and citation matching.
