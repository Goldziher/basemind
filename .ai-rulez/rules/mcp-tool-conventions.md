---
priority: high
---

# MCP Tool Conventions

The surface is nine domain tools, not one tool per verb: `code`, `graph`, `git`, `memory`, `web`, `agents`, `workspace`, `shell`, `admin`. Adding a capability means adding a **mode** to an existing domain, never a tenth top-level tool — every extra name competes for the same keyword search in hosts that defer tools, and GH #50 showed one bad schema takes the whole registry down with it. Skipping a step below leaves the mode half-wired; see the `mcp-tool-checklist` skill for the full walkthrough.

1. **`src/mcp/mode.rs`** — add the variant to the domain's `define_mode!` block: the Rust name, the wire spelling, and a short meaning. This generates the enum, `ALL_MODES`, `parse()`, `telemetry_key()`, and a flat `{"type":"string","enum":[…]}` schema. Never `#[derive(JsonSchema)]` on a doc-commented enum — that emits `oneOf`, which silently dropped the entire registry in GH #50.
2. **`src/mcp/types_<domain>.rs`** — add any new parameter as an **optional sibling** on the domain's flat `<Domain>Params` struct (`Option<T>` + `#[serde(default)]`). Never a nested struct or a schema union — `tests/mcp_schema_wire.rs` forbids `$ref` / `$defs` / `oneOf` / `anyOf` / `allOf`. Reuse `RelPath` for path fields; do not accept arbitrary `String` paths.
3. **`src/mcp/helpers_<domain>.rs`** (or the matching area slice, e.g. `helpers_calls.rs` / `helpers_graph.rs` / `helpers_grep.rs`) — add the dispatch arm in `run_<domain>` and the mode's accepted fields to the per-mode allow-list. The allow-list is inverted deliberately: a field belonging to another mode must be rejected, not ignored — an ignored parameter reads to an agent as a successful call that honoured it.
4. **`src/cli/<domain>.rs`** — add the matching clap subcommand. Parity is a strict bijection enforced by `tests/cli_parity.rs`, which walks `mode::domain_modes()`.
5. **`tests/cli_parity.rs`** — add the `(tool, Some("mode"), "cli path")` row.
6. **`tests/mcp_smoke.rs`** — add an assertion to that domain's per-mode coverage test and its mode-validation test: the mode appears in the advertised inputSchema, a foreign field is refused, and a missing required field names the pair.
7. **`tests/harden.rs`** — add the mode to the per-repo sweep loop, and a canary lower bound (`>=`, never equality) when one is meaningful.
8. **`README.md`** — extend the domain's row.

Mode descriptions should state the contract honestly: what matching semantics (substring vs prefix), what's resolved (scope-aware vs name-only), and what's capped (`scan_cap = limit * 8` for the index scanners). Agents make routing decisions from the description string — it is the retrieval surface, since hosts defer tools and surface them by keyword search.

Two costs consolidation imposes, not to be "fixed" locally: no `output_schema` on any of the nine tools (each domain's modes return different shapes; SEP-2106 allows one per tool), and annotations coarsen to the union of a domain's modes, resolving toward the side effect (`shell` advertises `destructive_hint` because `kill` is one of its modes). See ADR-0011.
