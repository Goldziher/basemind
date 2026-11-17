---
type: Decision
title: Tdd And Poly
x-ai-rulez:
  kind: rule
  id: tdd-and-poly
  metadata:
    priority: high
---

# TDD + poly Workflow

- Practice red-green-refactor. Write a failing test first when the change is observable from the public API or MCP surface.
- The unit-test bar is the harness in `tests/harden.rs` plus the smoke contract in `tests/mcp_smoke.rs`. A new MCP tool needs a smoke assertion (a working call plus its argument refusals), a `REQUIRED` entry in `tests/mcp_schema_wire.rs`, a `tests/cli_parity/capabilities.rs` row and (when sensible) a harden canary.
- Before every commit, run the local check triad:
  - `cargo fmt`
  - `cargo clippy --workspace --all-targets --tests -- -D warnings`
  - `cargo test --workspace`
  - `poly lint .` (catches typos, markdown line length, cargo-deny, cargo-machete, rustdoc-lint); the 1000-line cap is `tests/max_lines.rs`, run by `cargo test`
- Clippy is strict (`-D warnings`); do not silence with `#[allow(...)]` unless the warning is genuinely incorrect — and write a one-line `//` comment explaining why when you do.
- Code generation lives in `build.rs` and JSON schemas under `schema/`. Hand-editing generated files is forbidden; regenerate via the build.
- Commits use Conventional Commit prefixes (`feat:`, `fix:`, `perf:`, `chore:`, `refactor:`). Match the style in `git log`.
