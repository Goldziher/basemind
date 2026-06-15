---
priority: critical
---

# Module Size Cap

- Every file under `src/**/*.rs` is capped at **1000 lines** by the prek hook `rust-max-lines` (see `.pre-commit-config.yaml`).
- When a file approaches the cap, refactor by extracting helpers, types, or submodules — do not raise the cap.
- The cap exists because basemind already has precedent files that were split (`src/mcp/tools.rs` → `tools.rs` + `tools_<area>.rs` + `helpers.rs` + `helpers_<area>.rs`) and the split shape is the project's preferred unit of work.
- Bodies of `#[tool]` methods on `tools.rs` (and the `tools_<area>.rs` siblings) should be thin wrappers around helpers in `src/mcp/helpers*.rs`; that keeps each file under the cap as the MCP surface grows.
