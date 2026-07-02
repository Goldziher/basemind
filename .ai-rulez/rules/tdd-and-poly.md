---
priority: high
---

# TDD + poly Workflow

- Practice red-green-refactor. Write a failing test first when the change is observable from the public API or MCP surface.
- The unit-test bar is the harness in `tests/harden.rs` plus the smoke contract in `tests/mcp_smoke.rs`. New MCP tools need a smoke assertion and (when sensible) a harden canary.
- Before every commit, run the local check triad:
  - `cargo fmt`
  - `cargo clippy --workspace --all-targets --tests -- -D warnings`
  - `cargo test --workspace`
  - `poly lint .` (catches typos, markdown line length, cargo-deny, cargo-machete, rustdoc-lint, rust-max-lines)
- Clippy is strict (`-D warnings`); do not silence with `#[allow(...)]` unless the warning is genuinely incorrect — and write a one-line `//` comment explaining why when you do.
- Code generation lives in `build.rs` and JSON schemas under `schema/`. Hand-editing generated files is forbidden; regenerate via the build.
- Commits use Conventional Commit prefixes (`feat:`, `fix:`, `perf:`, `chore:`, `refactor:`). Match the style in `git log`.
