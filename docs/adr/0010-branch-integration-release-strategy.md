# ADR-0010: Branch, integration & release strategy

- **Status:** Proposed
- **Date:** 2026-08-02
- **Deciders:** basemind maintainers
- **Related:** ADR-0001 … ADR-0009 (all capabilities delivered under this roadmap),
  ADR-0006 (interactive UI), ADR-0007 (agent-launchable display)

## Context

This roadmap spans many sessions and two natures of change: **index/graph capabilities** that belong
in the core product (the unified graph, provenance, traversal, communities, headless rendering,
doc↔code links, rationale nodes), and **UI capabilities** that drag in a new GUI toolchain (the
desktop app and the agent display tool). Coupling them at the same maturity would either hold core
capability hostage to GUI stability or push an unfinished GUI into default builds and releases.

The UI work also has a natural home: the agent-layer branch already carries the transport seam the
UI depends on. We need a standing decision on where each kind of work lands and how it ships, so
individual sessions don't re-litigate it each time.

## Decision

- **Land the index/graph/rendering capabilities as core, main-mergeable work** as each stabilizes
  (ADR-0001 through ADR-0005, ADR-0008, ADR-0009). They extend the product and should not wait on the
  GUI.
- **Keep the interactive UI work on the agent-layer branch** until it is stable, then merge (ADR-0006,
  ADR-0007). It carries the new GUI toolchain and depends on the branch's transport seam.
- **Feature-gate by cost:** traversal and rendering in default builds; the **UI behind a
  crate/feature** so default builds and the release matrix are unaffected until opted in; audio/video
  transcription opt-in (ADR-0008).
- **Version and release in lockstep** via basemind's single sanctioned version-bump workflow; the
  desktop app joins the release archive. A docker image is optional and follows the deferred web mode
  (ADR-0006).

## Consequences

- Core graph capability ships continuously without being gated on GUI maturity.
- The UI matures on a branch without destabilizing releases, and there is a clear per-session rule for
  where new work goes.
- Trade-off: temporary divergence between the branch and main for UI work — mitigated by landing the
  shared substrate (graph + rendering) on main first, so the branch only carries the thin interactive
  shell over it.

## Alternatives considered

- **Do everything on the branch until one large merge.** Rejected: long-lived divergence and a
  painful integration; core capability would ship late for no reason.
- **Everything straight to main, including a half-built GUI.** Rejected: destabilizes releases and
  drags an unfinished GUI toolchain into default builds.
- **A separate repository for the UI.** Rejected: splits the transport seam and breaks the release
  version lockstep the project relies on.
