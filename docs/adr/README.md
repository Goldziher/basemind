# Architecture Decision Records

This directory holds basemind's Architecture Decision Records (ADRs): short documents that capture
a single significant architectural decision, the context that forced it, and its consequences.

## Why ADRs

basemind is built across many sessions by both humans and agents. An ADR makes each load-bearing
decision — and the alternatives we rejected — durable and greppable, so later work builds on the
decision instead of re-litigating it. They complement [`../ARCHITECTURE.md`](../ARCHITECTURE.md)
(which describes the system as it is now) by recording *why* it is that way.

## Format

Each ADR is one file, `NNNN-<kebab-slug>.md`, numbered in creation order. Copy
[`0000-template.md`](0000-template.md) to start a new one. The template follows the
Nygard/MADR shape:

- **Title** — `# ADR-NNNN: <decision>`
- **Status** — `Proposed` → `Accepted` → (optionally) `Superseded by ADR-MMMM` / `Deprecated`
- **Context** — the forces at play: the problem, constraints, and what exists today
- **Decision** — what we will do (active voice, present tense)
- **Consequences** — what becomes easier and harder as a result; follow-on work
- **Alternatives considered** — the options we rejected and why

## Lifecycle

1. Author the ADR as **Proposed**.
2. A maintainer (human) reviews and moves it to **Accepted** (or requests changes).
3. Implement against the Accepted decision. If a later ADR overrides it, mark the old one
   **Superseded by ADR-MMMM** rather than editing its decision.

Keep ADRs short — a decision, not a design doc. Link related ADRs by number.

## Index

The ADRs below (0001–0010) form one roadmap: give basemind a knowledge-graph capability layer
(typed graph, traversal, communities), a rendering engine, an interactive UI, and document/rationale
graph edges. They are sequenced by dependency — foundations first, then the capabilities built on
them.

| ADR | Title | Status |
|---|---|---|
| [0000](0000-record-architecture-decisions.md) | Record architecture decisions | Accepted |
| [0001](0001-unified-typed-code-graph.md) | Unified typed code-graph model | Accepted |
| [0002](0002-edge-provenance-and-confidence.md) | Edge provenance and confidence | Accepted |
| [0003](0003-graph-traversal-tools.md) | Graph traversal capabilities | Accepted |
| [0004](0004-community-detection.md) | Community detection and deterministic labels | Proposed |
| [0005](0005-rendering-engine.md) | Rendering engine — one payload, pluggable renderers | Proposed |
| [0006](0006-interactive-ui-tauri.md) | Interactive UI — Tauri desktop app | Proposed |
| [0007](0007-agent-launchable-display-tool.md) | Agent-launchable UI display tool | Proposed |
| [0008](0008-documents-code-graph.md) | Documents ↔ code graph | Proposed |
| [0009](0009-rationale-decision-nodes.md) | Rationale / decision nodes | Proposed |
| [0010](0010-branch-integration-release-strategy.md) | Branch, integration & release strategy | Proposed |
