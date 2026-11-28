---
type: Decision
title: Module Size Cap
x-ai-rulez:
  kind: rule
  id: module-size-cap
  metadata:
    priority: critical
---

# Module Size Cap

- Every file under `src/**/*.rs` is capped at **1000 lines** by `tests/max_lines.rs`, which `cargo test` runs (poly cannot count lines).
- When a file approaches the cap, refactor by extracting helpers, types, or submodules — do not raise the cap.
- The cap exists because basemind already has precedent files that were split (`src/mcp/tools.rs` → `tools.rs` + `tools_<area>.rs`, the bodies into one file per tool under `crates/basemind-engine/src/engine/ops/`) and the split shape is the project's preferred unit of work.
- `#[tool]` methods on `tools.rs` (and the `tools_<area>.rs` siblings) are thin shims over an engine operation in `crates/basemind-engine/src/engine/ops/` or `crates/basemind-engine/src/engine/git/` (ADR-0039); that keeps each file under the cap as the MCP surface grows.
