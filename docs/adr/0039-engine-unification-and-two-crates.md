---
status: proposed
date: 2026-11-17
amends: ["0023", "0032"]
---

# ADR-0039: Engine unification first, then two crates

- **Deciders:** the repository owner
- **Related:** amends ADR-0023 and ADR-0032; ADR-0014, ADR-0018, ADR-0029

## Context

ADR-0023 splits the library into four crates (core, retrieval, engine, `basemind`) and ADR-0032
records the module table and the open edges as built. Three facts have changed since:

- `xberg` 1.4.0 resolves from the crates.io registry. ADR-0023 kept retrieval in its own crate
  partly because `xberg` was a git dependency that could not publish.
- Nothing core depends on pulls in tokio, rmcp, hyper, clap, lancedb or xberg. The only network
  stack below the root is `ureq`/`rustls`, through the tree-sitter language pack. The retrieval
  modules (`src/lance`, `src/embeddings.rs`) expose synchronous functions only; their runtimes are
  internal.
- `tests/layering.rs` already enforces direction between core, retrieval and engine inside one
  crate. A crate boundary between core and retrieval would widen every core `pub(crate)` item that
  retrieval reaches for no embedder benefit.

The engine named in ADR-0023 does not exist yet. The read model the tools share lives in
`src/mcp`: `SharedReadStack` behind every `ServerState`, an asynchronous tokio `RwLock<Store>`, a
tokio `Notify` for the map warm, and the connection topology (`host`, `daemon_writer`) and the
telemetry writer on the same struct. Tool bodies return `CallToolResult` and `McpError`. Until that
is a synchronous engine with typed operations, there is nothing to extract.

## Decision

ADR-0023 and ADR-0032 stand except where this record changes them.

1. **Two crates, not four.**

   | Crate | Holds | Dependencies |
   |---|---|---|
   | `basemind-engine` | every `LAYERS` entry of layer core, retrieval or engine, and `src/engine/` | tokio (runtime only), xberg and lancedb only behind the `code-search`, `documents` and `memory` features; never rmcp or clap |
   | `basemind` | `cli`, `mcp` (the adapter, telemetry, tasks), `comms`, `watcher`, `eval`, `render`, `testing`, `config::overrides`, the binaries | rmcp, clap, hyper, tokio (full), `basemind-engine` |

   The core, retrieval and engine layers stay as layers inside `basemind-engine`, enforced by
   `tests/layering.rs` exactly as today. This replaces ADR-0023's four-crate table and its rule 4
   publish order: the vendored forks, then `basemind-engine`, then `basemind`.
2. **Engine unification comes before extraction.** The read model moves out of `src/mcp` into
   `src/engine` while it is still one crate, one scripted step at a time, each with no behaviour
   change:
   - `engine::Workspace` (formerly `SharedReadStack`) holds the store, the in-RAM map, the git
     handles, the caches and the lifecycle flags of one `(root, view)`. It is synchronous: the
     store lock is a `parking_lot::RwLock`, the warm barrier a `Mutex`/`Condvar` gate, the lazy
     map build a `OnceLock`. Async callers in `src/mcp` never wait on it on a runtime worker: a
     contended store lock is taken on a blocking thread, and the warm wait runs in the existing
     `block_in_place` seam.
   - `parking_lot` rather than `std::sync::RwLock` because it is task-fair and does not poison,
     as the tokio lock it replaces was, and because its owned guards (`arc_lock`, `send_guard`)
     let the adapter calls that still hold a read across an `.await` (callers and goto-definition
     over a daemon forward) keep doing so until those forwards are deleted. A `std` guard is not
     `Send`, so those calls would have had to drop and retake the read mid-call, which is a
     behaviour change under a concurrent scan.
   - Per-connection and per-host state stays in the adapter: `ServerState` keeps the telemetry
     writer, the daemon topology (`daemon_writer`, the hosted pool seam) and the session identity.
   - Next, an `EngineError` and one synchronous operation per verb tool, with the MCP shims and
     the CLI as two adapters over the same operation (ADR-0023 rule 7).
   - Processes that do not own the index then forward whole tool calls, so the in-process
     forwarding seams (`HostBackend`, `HistoryHost`, `IndexRoute` and its in-RAM fallback) are
     deleted rather than moved.
3. **`src/engine` names no rmcp, clap or tokio type.** `tests/layering.rs` places `engine` in the
   engine layer and forbids those crates there. Edges the unification has not cut yet are listed
   exactly in `KNOWN_VIOLATIONS`, each with the step that removes it (ADR-0032 rule 3 extended):

   | Edge | Removed by |
   |---|---|
   | `src/engine/map_cache.rs -> mcp::helpers_calls`, `-> mcp::helpers_impls` (the in-RAM call and implementation projections) | the deletion of the in-RAM fallback |
   | `src/engine/workspace.rs -> extern tokio` (the embedder's async `OnceCell`) | the memory operations step |
   | `src/engine/workspace.rs -> mcp::identity`, `-> mcp::lance_cell`, `-> mcp::memory_vectors` (the memory owner, the LanceDB cells, the vector backfill) | the memory operations step |
   | `src/engine/workspace.rs -> mcp::helpers_files` (`TrackedCache`) | the `find_files` operation step |

   Extraction cannot start while any of these remains, in addition to ADR-0032's three edges.
4. **A CLI command forwards its whole call to the daemon.** With the daemon up, a tool command on
   the working view opens no store. It opens one relay session, sends `tools/call {name, arguments}`
   and renders the `CallToolResult` the daemon's own tool body answered:
   - The relay hello names its client: `serve` (the default, an agent's session) or `cli`. A CLI
     session answers a failed call as a JSON-RPC error that keeps its code and data, so the exit
     status is the one an in-process call gives. It also answers the CLI-only commands (`git churn`,
     `git by-path`, `code chunk`, `memory delete`) by name. They stay off `tools/list`, which is the
     same for both kinds. The kind is the peer's own claim, so only the daemon's local socket or
     pipe honours it. The HTTP front-end never builds a CLI session.
   - The kind is an additive field in `RelayHello`, and the daemon's acknowledgement (`cli`) is an
     additive field in `RelayWelcome`. `RELAY_PROTO_VER` stays 2. An older relay's hello reads as
     `serve`, and an older daemon ignores the kind. A CLI that gets no acknowledgement refuses with
     exit 4 and names `basemind comms stop`. It never reads an older daemon's `isError` answers as
     successes. A relay protocol bump would have made every older `serve` refuse a newer daemon of
     its generation, which ADR-0024 rule 2 has it use as it is.
   - A view other than the working tree runs in process, as the daemon hosts only the working view.
     Its git-history tools live-walk while the daemon is up. Fjall locks the database directory
     exclusively on every open, readers included (`try_lock` in fjall 3.1's `locked_file.rs`), and
     the daemon holds `git-history.fjall/`. On this repository (2,349 commits), `git log --path
     src/store.rs` (95 commits) takes a p50 of 180 ms that way, against 32 ms when the read was
     forwarded over the daemon socket and 22 ms for the forwarded working-view call.
   - `admin telemetry` and `admin eval` are not tool calls and run in process.

## Consequences

- An embedder takes one crate, `basemind-engine` with no default features, and gets core and the
  engine without tokio, rmcp, clap, lancedb or xberg. The layering test, plus a `cargo tree` check
  added at extraction, keeps it that way.
- Nothing stops a core module from naming tokio once a retrieval feature is on, except the
  layering test's forbidden-crate check. That check is the boundary ADR-0023 wanted from a crate
  line.
- The unification steps touch shared state used by every tool. Each lands as a scripted,
  re-runnable move with the compiler, the layering test and the full suite as the gate.
- A read that waits on a scan now waits on a blocking-pool thread rather than as a parked task;
  which reads wait, and for how long, is unchanged. `parking_lot` is already in the dependency
  graph; the root crate now names it directly.

## Alternatives considered

- **Keep ADR-0023's four crates.** Rejected: its publishing reason is gone, and the extra boundary
  would widen hundreds of core items to `pub` and put the scan's retrieval seams across two crate
  lines, for an embedder that gets the same dependency graph from features.
- **Extract first, unify later.** Rejected: extraction would move `src/mcp` tool bodies that still
  return `CallToolResult` into the root crate, and every later engine operation would cross a crate
  line while being written.
- **Keep an async engine API.** Rejected: ADR-0023 rule 3 keeps async out of the public API, and the
  retrieval functions it would wrap are already synchronous.
