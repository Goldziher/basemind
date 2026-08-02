# ADR-0000: Record architecture decisions

- **Status:** Accepted
- **Date:** 2026-08-02
- **Deciders:** basemind maintainers
- **Related:** none

## Context

basemind is entering a multi-session effort to grow a full knowledge-graph capability set (typed +
confidence-tagged edges, graph traversal, community detection, a rendering engine, an interactive
Tauri UI, doc↔code links, and rationale nodes) on top of the existing tree-sitter code map. The work
spans many sessions and multiple contributors (human and agent), and each phase makes load-bearing
architectural choices that later phases depend on.

Until now the only architecture documentation has been `docs/ARCHITECTURE.md` (a snapshot of the
current design) and the `.ai-rulez/context/*` briefs. Neither captures *why* a decision was made or
which alternatives were rejected, so decisions risk being silently re-litigated or contradicted
across sessions.

## Decision

We adopt Architecture Decision Records, stored in `docs/adr/` as numbered Markdown files
(`NNNN-<slug>.md`) following the Nygard/MADR template in `docs/adr/0000-template.md`. Each ADR
records one decision with its Context, Decision, Consequences, and rejected Alternatives, and moves
through `Proposed → Accepted` (a human maintainer accepts). Superseding decisions get a new ADR and
mark the old one `Superseded by ADR-MMMM` rather than rewriting it. `docs/adr/README.md` carries the
index and the process.

## Consequences

- Every significant decision in the knowledge-graph effort (and beyond) becomes durable, greppable,
  and attributable; new sessions read the ADR instead of reverse-engineering intent.
- A small authoring cost per decision, and an index to keep current.
- `docs/ARCHITECTURE.md` continues to describe the system as-built; ADRs explain the why and the
  path not taken. A pointer from `ARCHITECTURE.md` links the two.

## Alternatives considered

- **Keep decisions in commit messages / PR descriptions only.** Rejected: not discoverable as a set,
  and easily lost when squash-merging or when an agent lacks the git context.
- **A single growing `DECISIONS.md`.** Rejected: merge-conflict-prone across parallel sessions and
  hard to mark individual decisions superseded; one-file-per-decision is the point.
- **Put decisions in `.ai-rulez/context/`.** Rejected: that surface is generated/rules-oriented and
  describes current conventions, not the reasoning or the rejected options behind them.
