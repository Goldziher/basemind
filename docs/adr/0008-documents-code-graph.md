# ADR-0008: Documents ↔ code graph

- **Status:** Accepted (implemented on `feat/agent-layer`, 2026-08-04)
- **Date:** 2026-08-02
- **Deciders:** basemind maintainers
- **Related:** ADR-0001 (unified typed code-graph), ADR-0002 (edge provenance + confidence),
  ADR-0009 (rationale / decision nodes)

## Implementation note

Landed feature-gated behind `documents` as `NodeKey::DocChunk` + `EdgeKind::Documents` and a read-side
`documents` build lane in `src/mcp/codegraph.rs`. At document-scan time each chunk's keyword/entity
name mentions and path-like citations are written to a vector-free LanceDB `doc_links` table
(`src/scanner_doc_links.rs`, `src/lance/doc_links.rs`) — persisted in the **document store, not the code
index**, so ADR-0008 required **no `RELEASE_MINOR` bump**. The serve cache-warm path reloads the links
into `MapCache.doc_links` on a non-reactor thread (avoiding the `LanceStore` `block_on`-in-reactor
hazard). Resolution is deferred to the build lane: a name mention resolves against the repo symbol table
(INFERRED/AMBIGUOUS/virtual), a path citation to an indexed file is EXTRACTED (else INFERRED). Deferred
per the ADR: audio/video transcription and model-based entity linking — the producer is a deterministic
text heuristic.

## Context

basemind already indexes documents — PDF, Office, HTML, and images via OCR — into a searchable RAG
corpus. But that corpus is an island: there are no edges between a design doc and the code it
describes. A knowledge graph that unifies prose and code lets you traverse from a spec paragraph to
the function that implements it, and from a function back to the document that explains why it exists.
Comparable tools don't do this well; it's a genuine differentiator and one of the two stretch goals.

## Decision

**Promote document chunks to nodes in the unified graph (ADR-0001) and add doc→symbol edges** by
resolving mentions and citations in document text — symbol names and path references — to code nodes,
each edge **provenance/confidence-tagged per ADR-0002** (a resolved path/symbol reference is stronger
than a bare name mention). This unifies the RAG corpus with the code map behind one traversal
(ADR-0003) and one rendering (ADR-0005) surface.

**Persist, don't derive.** Resolving symbol and path mentions across document text is extraction work
as costly per query as rationale extraction (ADR-0009) — so, unlike the resolution signals ADR-0002
derives on read, doc→symbol links are computed once at document-scan time and stored. They live in the
document store alongside the chunks they annotate (rebuilt on document rescan and versioned with that
store), so this does **not** touch the code index's schema version; the code-index node-kind bump is
ADR-0009's, not this one.

**Audio/video transcription stays deferred** — it is a large dependency for marginal near-term value.
We link the text, Office, HTML, and image-OCR corpus that basemind already extracts.

## Consequences

- Traversal and the UI span documents and code: "what implements this spec", "what does this
  function's design doc say" become graph queries.
- The document corpus stops being a silo and joins the same graph everything else reads.
- Trade-off: mention resolution is inherently noisier than code resolution. Confidence tagging
  (ADR-0002) is what keeps it honest — AMBIGUOUS doc links must render visibly weaker than proven
  code edges.

## Alternatives considered

- **Keep documents search-only, with no graph edges.** Rejected: that is the status quo and misses
  the differentiator entirely.
- **Require explicit machine-readable links inside documents.** Rejected: too much authoring burden
  and near-zero real-world coverage.
- **Full model-based entity linking.** Rejected for now: heavy and non-deterministic; start with
  deterministic name/path mention resolution and revisit if coverage proves insufficient.
