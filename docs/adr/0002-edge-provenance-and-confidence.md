# ADR-0002: Edge provenance and confidence

- **Status:** Proposed
- **Date:** 2026-08-02
- **Deciders:** basemind maintainers
- **Related:** ADR-0001 (unified typed code-graph), ADR-0003 (traversal), ADR-0005 (rendering engine)

## Context

Not every edge in a code graph is equally trustworthy. An edge proven by scope and import resolution
— we know exactly which definition a use binds to — is a stronger fact than an edge matched only by
name, which is stronger than a purely heuristic guess. Knowledge-graph tools in this space make that
distinction explicit: they tag every edge with a provenance/confidence label (EXTRACTED / INFERRED /
AMBIGUOUS) and a numeric score, so a reader can weight a proven binding differently from a name
match and a renderer can style them differently.

basemind already *computes* this distinction — it knows when a reference has been resolved to a
specific definition, when it only matched a name, and when a relationship is a pure heuristic — but
it never surfaces that as a first-class, uniform signal. Fragments leak out of individual
capabilities (one lookup reports whether a hit was resolved), with no shared model. Resolution
coverage is also legitimately uneven: intra-file resolution exists for every language, cross-file
resolution is proven for some languages and name-only for others. So the *same* kind of edge can be
proven in one place and merely inferred in another — a real property we should report honestly, not
hide.

The unified graph (ADR-0001) needs one consistent provenance model so that traversal (weighting),
rendering (styling), and export interop all read the same signal.

## Decision

Attach a **three-level provenance tag plus a numeric confidence** to every edge of the unified graph,
**derived at query time from the resolution state basemind already computes — no new persisted bit
and no schema bump:**

- **EXTRACTED** — the edge's *target node* is proven: a resolved use→definition binding, or an edge
  whose target is structurally unambiguous (a containment/nesting edge). Note the subtlety: the
  *existence* of an import or an inheritance is explicit in the source, but *which definition it binds
  to* is not — so a bare import/inherit is **not** EXTRACTED unless its target has actually been
  resolved.
- **INFERRED** — the target matched by name but resolution did not prove it: name-level call edges,
  and import/inherit edges bound to a definition only by name.
- **AMBIGUOUS** — heuristic or one-name-to-many: a substring/heuristic relationship, or a name that
  resolves to several candidate definitions.

Alongside the tag, carry a numeric confidence on a small fixed ladder (EXTRACTED = 1.0,
INFERRED = 0.5, AMBIGUOUS = 0.2), primarily as a default edge weight for traversal (ADR-0003) and a
styling signal for the renderer (ADR-0005). It also lines up with the common graph-exchange formats —
a convenient interop bonus, not the reason for the ladder.

As part of this we **emit the import and inherit edge kinds** into the graph and the architecture map.
Their *records* already exist in the index, but only as names (a module name; a trait/impl name) with
no resolved target — so emitting them as edges-to-nodes means resolving those names first, and the
resulting edges are INFERRED (or AMBIGUOUS for multi-definition names) by construction. This is real
resolution work, not a free surfacing step.

Provenance is **derived on read** from the resolved-reference index. That index is not reachable in
every serve mode: a read-only or daemon-writer server reaches the resolved cross-file bindings only
by forwarding to the daemon, and a degraded read-only fallback sees intra-file bindings only. The
graph must therefore either forward provenance derivation the same way reads are forwarded, or label
honestly — an edge whose proof is unreachable in the current mode is reported as INFERRED, never
falsely EXTRACTED. Only if profiling later shows the derivation is too costly on the hot path do we
persist a confidence bit — a separate change gated on a schema version bump and its wipe-and-rescan
migration, noted in the changelog per project policy. This ADR does not take that step.

## Consequences

- Every consumer of the graph gets one uniform provenance signal: traversal can prefer proven paths
  (weight = inverse confidence), the renderer can style EXTRACTED solid / INFERRED dashed /
  AMBIGUOUS faint, and exports match the field names the ecosystem already uses.
- The confidence of a given edge can legitimately differ across files and languages, reflecting real
  resolution coverage. This is honest, but every tool description and UI affordance must state that
  INFERRED means *unproven*, not *wrong*.
- Provenance accuracy is serve-mode-dependent: outside the writer, cross-file EXTRACTED can degrade to
  INFERRED unless derivation is forwarded to the daemon. The rule is that the graph degrades *down*
  (proof unreachable → INFERRED), never up — so a reported EXTRACTED is always trustworthy, and the
  only failure mode is under-claiming confidence, not over-claiming it.
- No migration or wipe now; a persisted-confidence optimization stays available behind a future
  schema bump if measurement demands it.

## Alternatives considered

- **A two-state resolved/unresolved flag only.** Rejected: it collapses genuinely heuristic edges
  into "unresolved" and loses the AMBIGUOUS signal that readers and the renderer want; the
  three-level model matches the ecosystem and costs nothing extra to derive.
- **Persist a confidence value on every edge now.** Rejected as premature optimization: it forces a
  schema bump and wipe for a value we can derive from state we already compute; revisit only if
  measured hot-path cost demands it.
- **Free-form per-language / per-heuristic numeric scores.** Rejected: harder to reason about and to
  map to renderer styling; a small fixed ladder tied to the three tags is enough and is interoperable
  with existing graph consumers.
