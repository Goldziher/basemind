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

Every graph capability (ADR-0003) gains a format/export option, and an export can be written to
basemind's machine-global cache (opt-in per call, under a content-addressed name). This engine is
the **single rendering path** shared by the headless tools and by the desktop UI (ADR-0006).

## Consequences

- Visual and export parity with the ecosystem, achieved cheaply and **offline**; the interactive HTML
  works with no network.
- The UI (ADR-0006) becomes a thin interactive shell over a payload that already renders headless —
  no separate rendering stack for the GUI.
- Deterministic outputs are snapshot-testable; interop is a diff against the standard node-link shape.
- Shipped as the `graph_export` MCP tool over the canonical `GraphView` payload: the **text/machine
  renderers** (node-link JSON, DOT, Mermaid, GraphML, Cypher) plus the **self-contained, offline
  interactive HTML page** (`format: "html"`) — all pure, deterministic, and offline. The interactive
  page carries a **zero-dependency** vanilla-JS canvas engine (pan/zoom/search/community legend)
  inlined into a single document; no CDN and no vendored third-party library, so the file works
  straight off disk. This is the shared artifact the agent-launchable display (ADR-0007) opens.
  A **static SVG picture** (`format: "svg"`) completes the renderer set: it bakes the *same*
  deterministic force layout the HTML engine runs in the browser — identical constants, no
  randomness — into resolved `<line>`/`<circle>` geometry server-side, with community colors baked
  HSL→RGB so it renders in plain SVG viewers. `graph_export` can also **write** the rendered content
  to the machine-global cache (`write: true` → `<workspace-cache>/exports/graph-<hash>.<ext>`,
  content-addressed so there is no caller-supplied path component) and return the absolute
  `output_path`; writing is opt-in so the inline-content contract is unchanged by default.
  **Still deferred:** the Tauri desktop shell (ADR-0006).
- Trade-off resolved by **not vendoring a third-party library at all**: the interactive page ships a
  hand-rolled, zero-dependency vanilla-JS canvas engine instead. This keeps the artifact fully
  self-contained and sidesteps the offline-vs-size-vs-capability tension a vendored library would have
  forced, at the cost of a simpler force layout than a mature graph library. The `max_nodes` cap
  bounds the client-side O(n²) layout; the community meta-graph fallback (ADR-0004) remains the answer
  for graphs too large to draw node-for-node.

## Alternatives considered

- **CDN-loaded interactive HTML, like comparable tools.** Rejected outright: violates basemind's
  offline-first guarantee.
- **Server-only rendering, no static exports.** Rejected: loses the agent, CI, and interop use cases;
  a self-contained file is the most portable artifact.
- **A bespoke wire format for the payload.** Rejected: mirror the common node-link shape so exports
  interoperate with existing graph consumers for free.
