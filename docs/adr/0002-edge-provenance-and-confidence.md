# ADR-0002: Edge provenance and confidence

- **Status:** Proposed
- **Date:** 2026-08-02
- **Deciders:** basemind maintainers
- **Related:** ADR-0001 (unified typed code-graph), ADR-0003 (traversal), ADR-0005 (rendering engine)

## Context

Competing knowledge-graph tools (e.g. graphify) tag every edge with a confidence/provenance label —
`EXTRACTED` (explicit in the source), `INFERRED` (derived by resolution), `AMBIGUOUS` (uncertain) —
and a numeric `confidence_score`. This lets a reader trust a call edge proven by scope resolution
differently from one matched only by name, and lets the renderer style them differently (solid vs
dashed).

basemind does not surface this today, but it already *computes the distinction*:

- `find_callers` refines each call hit with a `resolved: bool` and reports a `resolved_total`
  (`src/mcp/helpers_calls.rs`), where `resolved: true` means scope/import resolution **proved** the
  binding via the `refs_by_def` keyspace, and `false` means only a name-level match in
  `calls_by_callee`. This is a latent two-state provenance signal exposed on one tool.
- Resolution coverage is uneven by construction: intra-file resolution exists for all languages
  (tree-sitter `locals`), cross-file resolution exists for JS/TS (oxc) and is name-only elsewhere
  (see the cross-file-resolution coverage notes). So the same edge kind can be EXTRACTED in one file
  and INFERRED in another.
- `import`/`inherit` edges are name-level joins (`imports_by_module`, `implementations_by_trait`),
  and `dependents` is an explicit substring heuristic — a natural AMBIGUOUS tier.

There is no `confidence` field on any edge struct today, and `ArchEdge.kind` is always `"calls"`
(ADR-0001). We need a single, consistent provenance model for the unified graph so traversal
(weighting), the renderer (styling), and export interop all read the same signal.

## Decision

Adopt a three-level edge provenance tag on the unified `codegraph` (ADR-0001), derived at query time
from signals basemind already stores — **no new stored bit, no schema bump**:

- **EXTRACTED** — resolution-proven: the edge is backed by a `refs_by_def` resolved use→def binding
  (the `resolved: true` case), or is otherwise explicit in the source (e.g. a direct import
  statement, a `contains` nesting edge).
- **INFERRED** — name-level: call edges matched only via `calls_by_callee`, and import/inherit edges
  resolved by name against `symbols_by_name`, where resolution did not prove the target.
- **AMBIGUOUS** — heuristic or many-to-one: the `dependents` substring match, and name-level edges
  whose name resolves to multiple candidate definitions.

Alongside the tag we carry a `confidence_score: f32` (EXTRACTED = 1.0, INFERRED = 0.5,
AMBIGUOUS = 0.2) to mirror graphify's `graph.json` for export interop (ADR-0005) and to give
traversal a default edge weight (ADR-0003).

We also **emit the reserved `imports` and `inherits` edge lanes** into `architecture_map` and the
unified graph (they exist in the keyspaces; only the emit step is missing), each carrying its
provenance tag.

Provenance is **derived on read** from the existing keyspaces. Only if profiling shows the
derivation is too costly on the hot path do we persist a confidence bit on cross-file resolved edges
— and that would be a separate change gated on an `INDEX_SCHEMA_VER` bump per the
`index-keyspace-evolution` skill, with the wipe noted in the CHANGELOG. This ADR does not take that
step.

## Consequences

- Every consumer of `codegraph` gets a uniform provenance signal: traversal can prefer proven paths
  (Dijkstra weight = inverse confidence), the renderer can style EXTRACTED solid / INFERRED dashed /
  AMBIGUOUS faint, and `graph.json` export matches the field names competitors use.
- `find_callers`' existing `resolved`/`resolved_total` becomes the EXTRACTED anchor of a broader,
  consistent model rather than a one-off boolean.
- The confidence of a given edge can legitimately differ across files/languages (reflecting real
  resolution coverage); this is honest, but tool descriptions must state that INFERRED ≠ wrong, only
  unproven.
- No migration/wipe now. A future persisted-confidence optimization remains available behind a
  schema bump if needed.

## Alternatives considered

- **Two-state (resolved / unresolved) only.** Rejected: collapses genuinely heuristic edges
  (`dependents` substring, multi-def names) into "unresolved", losing the AMBIGUOUS signal that
  readers and the renderer want; the three-level model matches the ecosystem and costs nothing extra
  to derive.
- **Persist a confidence byte on every edge now.** Rejected as premature optimization: forces an
  index-schema bump and wipe for a value we can derive from data we already read; revisit only if
  measured hot-path cost demands it.
- **Free-form numeric scores per language/heuristic.** Rejected: harder to reason about and to map
  to renderer styling; a small fixed ladder (1.0 / 0.5 / 0.2) tied to the three tags is enough and
  is interoperable with existing `graph.json` consumers.
