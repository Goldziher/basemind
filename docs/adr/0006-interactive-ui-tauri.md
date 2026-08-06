# ADR-0006: Interactive UI — Tauri desktop app

- **Status:** Proposed
- **Date:** 2026-08-02
- **Deciders:** basemind maintainers
- **Related:** ADR-0005 (rendering engine), ADR-0007 (agent-launchable display),
  ADR-0010 (branch, integration & release strategy)

## Context

The user wants a real basemind UI: an interactive desktop window that renders the graph — a canvas
with live search, filters by community, edge kind, and confidence, and path highlighting — alongside
code-map and git-history panels, launched by a `basemind ui` subcommand. This goes well past the
static file comparable tools ship.

The agent-layer work was explicitly architected for this. Its client/event/command seam — the
abstraction the terminal UI already drives the agent engine through — was designed from the start as
the attach point for a desktop front-end; the desktop app was always the branch's later milestone.
basemind is also offline-first, so the UI must render with no network, and it already ships auxiliary
binaries (the terminal UI) in its release archive and launches them via a subcommand that execs the
sibling binary.

## Decision

Ship the UI as a **Tauri desktop app in its own crate**, driven through the **existing agent
client/event/command seam** (the same seam the terminal UI uses), rendering the canonical graph-view
payload (ADR-0005) **offline with vendored assets — no CDN**. A **`basemind ui` subcommand** launches
it by exec'ing the sibling app binary, mirroring how the terminal UI is launched, and it ships in the
release archive the same way.

A **dockerized web mode** — reusing basemind's existing HTTP server to serve the same offline
frontend to a browser — is recorded here as a **deliberate, deferred follow-on.** Tauri is primary
because it matches the designed seam and the desktop-window ask; the web/docker mode lands only when
there is demand.

## Consequences

- One interactive surface for graph + code-map + git-history, reusing a transport seam already built
  and tested rather than inventing a UI protocol.
- Offline and self-contained, consistent with basemind's guarantees.
- Release packaging gains one binary, launched by the established sibling-exec pattern.
- Trade-off: a GUI toolchain enters the build and release matrix — concretely a system webview
  dependency per platform (WebKitGTK on Linux, WebView2 on Windows), added to a release matrix that is
  already delicate with per-platform native-library packaging. ADR-0010 owns these platform
  dependencies; they are the main reason the UI is feature-gated and kept on the branch until stable,
  so default builds are unaffected until opted in.

## Alternatives considered

- **Terminal-only graph rendering.** Rejected: the existing terminal UI cannot host a rich,
  interactive graph canvas with search, filtering, and path highlighting.
- **A pure web app as the primary UI.** Rejected as primary: it needs a running server and a browser
  and does not match the desktop-window ask or the designed seam — kept instead as the deferred web
  mode.
- **Embed a browser control ad hoc, bypassing the seam.** Rejected: throws away the transport
  architecture the agent layer was built around and couples the UI to engine internals.

## Implementation note (first slice)

The **launch path and packaging** shipped first, carrying no GUI-toolchain blast radius:

- A new `crates/basemind-ui` workspace crate produces the sibling `basemind-ui` binary, and a
  `basemind ui` subcommand (`src/ui_cmd.rs`) re-execs it from beside `basemind` — the exact pattern
  `basemind agent` uses for `basemind-tui`. It tracks the release version in lock-step.
- **Neither front-end ships yet.** `scripts/package-release.sh` packages `basemind` alone, and the
  `basemind ui` / `basemind agent` launchers sit behind the root crate's `desktop-ui` / `agent-tui`
  features, which `full` omits — so a released binary never offers a command whose sibling is absent.
  Both go back into the archive when the window below lands and this ADR moves off **Proposed**.
- The interactive Tauri window — driving `basemind_agent::transport::AgentClient` and rendering the
  ADR-0005 graph-view payload offline — lands behind an off-by-default `desktop` cargo feature in a
  later slice, so `cargo build --workspace` / CI and default builds stay free of the per-platform
  webview dependency (ADR-0010).
- Until the window lands, `basemind ui` opens the offline, self-contained interactive HTML graph the
  `display` tool produces (ADR-0007) — the working baseline the resident window replaces.

Status stays **Proposed** until the interactive window and its seam bridge land.
