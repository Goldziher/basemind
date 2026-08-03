# ADR-0005: Rendering engine — one payload, pluggable renderers

- **Status:** Accepted
- **Date:** 2026-08-02
- **Deciders:** basemind maintainers
- **Related:** ADR-0001 (unified typed code-graph), ADR-0002 (edge provenance + confidence),
  ADR-0004 (community detection), ADR-0006 (interactive UI), ADR-0007 (agent-launchable display)

## Context

The visualization ambition is a real basemind UI (ADR-0006/0007), but the graph also has to be
renderable *headless*: for agents that want a picture or an export, for interoperability with other
graph tools, and — importantly — as the single serialized payload the UI itself consumes. Comparable
tools ship a static, CDN-loaded interactive HTML page plus a set of exports (node-link JSON, GraphML,
Cypher, SVG, Mermaid). basemind ships none of these today.

We want to match those exports and *beat* the offline story. basemind is offline-first and
self-contained; a rendering path that depends on fetching assets from a CDN at view time is
unacceptable. The rendering surface must also be deterministic and snapshot-testable.

## Decision

Define **one canonical graph-view payload** and a set of **pluggable renderers** over it:

- **The payload** is a superset of the common node-link exchange shape: nodes carry identity, label,
  location, kind, community and community label (ADR-0004), and centrality; edges carry their
  endpoints, kind, and provenance/confidence/weight (ADR-0002). This one payload is what every
  renderer *and the UI* consume.
- **Renderers:**
  - machine formats — node-link JSON (interop), DOT, Mermaid, and GraphML/Cypher for graph-database
    import;
  - a static picture — SVG;
  - an interactive view — a fully self-contained, **offline** HTML page with all assets vendored at
    build time (**no CDN**, unlike comparable tools).

Every graph capability (ADR-0003) gains a format/export option, and an export writes to basemind's
machine-global cache by default with an optional explicit path. This engine is the **single rendering
path** shared by the headless tools and by the desktop UI (ADR-0006).

## Consequences

- Visual and export parity with the ecosystem, achieved cheaply and **offline**; the interactive HTML
  works with no network.
- The UI (ADR-0006) becomes a thin interactive shell over a payload that already renders headless —
  no separate rendering stack for the GUI.
- Deterministic outputs are snapshot-testable; interop is a diff against the standard node-link shape.
- Shipped as the `graph_export` MCP tool over the canonical `GraphView` payload: the **text/machine
  renderers** (node-link JSON, DOT, Mermaid, GraphML, Cypher) — pure, deterministic, offline, no new
  dependencies. **Deferred to the UI ADRs (0006/0007):** the SVG picture and the self-contained
  interactive HTML page (the latter requires the vendored-JS library pick, which belongs with the
  desktop UI), plus writing exports to the machine-global cache (today `graph_export` returns the
  rendered content inline).
- Trade-off: vendoring interactive JS grows the build. The library choice is constrained on three axes
  at once — offline/self-contained, small enough to vendor, and capable enough to render thousands of
  nodes with live search and filtering — and these pull against each other. The specific library is a
  conscious pick made at implementation time, backstopped by the community meta-graph fallback
  (ADR-0004) when a graph is too large to draw node-for-node.

## Alternatives considered

- **CDN-loaded interactive HTML, like comparable tools.** Rejected outright: violates basemind's
  offline-first guarantee.
- **Server-only rendering, no static exports.** Rejected: loses the agent, CI, and interop use cases;
  a self-contained file is the most portable artifact.
- **A bespoke wire format for the payload.** Rejected: mirror the common node-link shape so exports
  interoperate with existing graph consumers for free.
