---
status: superseded
date: 2026-11-14
superseded-by: "0023"
---

# ADR-0019: Library crate layering

- **Status:** Superseded by ADR-0023 (2026-11-15)
- **Date:** 2026-11-14
- **Deciders:** basemind maintainers
- **Related:** ADR-0014, ADR-0017, ADR-0018

## Context

basemind ships as a CLI and MCP server and must also be embeddable as a library: a separate desktop
application will run the engine in-process. The current layout does not support that:

- One package is both library and binary, with 58 module declarations in `src/lib.rs` and a single
  re-export (`pub use config::Config`), so every module is public API and nothing is a facade.
- `rmcp`, `clap`, `tokio` and `jsonschema` are non-optional dependencies of that package.
- About 38k lines of read-side logic live under `src/mcp/` and return MCP result types. The CLI
  reaches them by calling MCP tool methods (ADR-0014).
- The document and vector stack pins a git dependency, so it cannot sit inside a crate that is
  published to crates.io.

## Decision

Split into four crates. Dependencies point downward only.

| Crate | Contents | Must not depend on |
|---|---|---|
| `basemind-core` | scan, store, index, extract, git, config, data-root injection, and the chunk-sink, embedder and vector-store traits | tokio, rmcp, clap, jsonschema |
| `basemind-retrieval` | embedding, the vector and lexical store implementation, chunk lanes; depends on core | |
| `basemind-engine` | `Workspace` / `Engine` API with typed synchronous operations and a typed error; optional dependency on retrieval | rmcp, clap |
| `basemind` | CLI, MCP adapters, comms, the binary | |

The root `basemind` crate re-exports the old module paths as deprecated for two minor releases. A
dependency test over `cargo metadata` asserts the table above, and is made to fail once on purpose
before it is trusted.

## Consequences

- The release-profile `opt-level` pins in `Cargo.toml` and the macOS link argument in `build.rs` do
  not propagate to a crate that embeds the engine. A build helper and a downstream-crate CI job
  that builds and runs scan, outline and references on macOS and Linux are required.
- Publish order is the vendored forks, then core, retrieval, engine and `basemind`.
- Retrieval sits outside the publishable core, so core can publish while the git-pinned document
  and vector stack is unresolved.
- ADR-0014's "the CLI runs the same tool code as MCP" stands.
- Moving the read-side logic out of `src/mcp/` is a large mechanical change; it lands as scripted
  moves with the compiler and the dependency test as the gate.

## Alternatives considered

- **Three crates with retrieval inside core** — rejected: core inherits the git-pinned dependency
  and stays unpublishable.
- **Separate `basemind-mcp` and `basemind-comms` crates** — rejected: they would form a dependency
  cycle with the engine and have no consumer.
