---
priority: medium
description: "End-to-end checklist for adding a mode to one of basemind's MCP domains"
---

# MCP Tool Checklist

Use this when extending basemind's MCP surface. Skipping a step leaves the tool half-wired.

**The surface is nine domain tools, not one tool per verb.** Adding a capability means adding a
**mode** to an existing domain — `code`, `graph`, `git`, `memory`, `web`, `agents`, `workspace`,
`shell`, `admin`. Adding a tenth top-level tool is a surface change that needs its own ADR, not a
checklist step: every extra name competes for the same keyword search in hosts that defer tools, and
GH #50 showed one bad schema takes the whole registry down with it.

Each domain owns a `types_<domain>.rs` / `tools_<domain>.rs` / `helpers_<domain>.rs` trio. Every
file stays under the **1000-line cap** enforced by `tests/max_lines.rs` — split into
`helpers_<domain>_<area>.rs` rather than raising it.

## Steps

1. **`src/mcp/mode.rs`** — add the variant to that domain's `define_mode!` block: the Rust name, the
   wire spelling, and a short meaning. The macro generates the enum, `ALL_MODES`, `parse()`,
   `telemetry_key()`, and a **flat** `{"type":"string","enum":[…]}` schema. Never
   `#[derive(JsonSchema)]` on a doc-commented enum — that emits `oneOf`, the construct that silently
   dropped the entire registry in GH #50. Drop the domain prefix the old flat name carried
   (`search_symbols` → `code` mode `symbols`); keep it only where the bare word would be ambiguous.

2. **`src/mcp/types_<domain>.rs`** — add any new parameter as an **optional sibling** on the
   domain's flat `<Domain>Params` struct (`Option<T>` + `#[serde(default)]`). Never a nested struct
   or a schema union: `tests/mcp_schema_wire.rs` forbids `$ref` / `$defs` / `oneOf` / `anyOf` /
   `allOf`, and schemars emits `$ref` for nested types. Paths are `RelPath`, never `String`. Limits
   default to 100 and cap at 1000.

3. **`src/mcp/helpers_<domain>.rs`** — add the dispatch arm in `run_<domain>`, and add the mode's
   accepted fields to the per-mode **allow-list**. The allow-list is inverted deliberately: with a
   dozen modes and dozens of sibling fields, an explicit per-mode *reject* list is where a newly
   added field silently becomes accept-everywhere. Use `require_field` for anything the mode cannot
   run without, so the error names the exact `mode`/field pair. A field belonging to another mode
   must be **rejected, not ignored** — an ignored parameter reads to an agent as a successful call
   that honoured it. Apply `scan_cap = limit * 8` when iterating an index range.

4. **`src/cli/<domain>.rs`** — add the matching clap subcommand. **Parity is a strict bijection and
   it is enforced**: `tests/cli_parity.rs` walks `mode::domain_modes()` and fails if any advertised
   mode has no resolving `basemind <domain> <mode> --help`. The CLI uses real subcommands, not
   `--mode`, so each gets its own `--help` and argument validation.

5. **`tests/cli_parity.rs`** — add the `(tool, Some("mode"), "cli path")` row. Keyed on the pair, not
   the tool name: keyed on names alone the test would verify nine tools and silently stop covering
   the modes beneath them.

6. **`tests/mcp_smoke.rs`** — add an assertion to that domain's per-mode coverage test, and to its
   `<domain>_tool_validates_every_mode_before_running_it` test: the mode appears in the advertised
   inputSchema, a foreign field is refused, and a missing required field names the pair. Derive the
   asserted wording from a real run; do not guess it.

7. **`tests/harden.rs`** — add the mode to the per-repo sweep loop. If a canonical canary exists
   (`find_references("spawn")` on tokio), assert a lower bound (`>=`), never equality — upstream
   repos churn.

8. **`README.md`** — extend the domain's row. One line, ≤ 120 chars (markdownlint cap).

## Two costs consolidation imposes — do not try to "fix" them locally

- **No `output_schema`** on a domain whose modes return different shapes. SEP-2106 allows exactly
  one per tool, and expressing a union means nested structs → `$ref` → dropped registry. Document
  the per-mode shapes in the description instead.
- **Annotations are the union of the domain's modes**, and the union resolves toward the side
  effect. If any mode writes or launches something, the whole tool advertises
  `read_only_hint: false` — a client that auto-approves read-only tools must not be able to trigger
  it. See ADR-0011.

## The description is a retrieval surface

Hosts defer MCP tools and surface them by keyword search, so with nine names the description **is**
how an agent finds the tool. It must carry the words someone actually types — "grep", "who calls
this", "find the definition", "read this instead of opening the file" — alongside the honest
contract: substring vs exact matching, scope-aware vs name-only resolution, and what is capped.
Server instructions are truncated at 2048 chars (`tests/mcp_schema_wire.rs`), so budget accordingly.

## Verification

- `cargo test --workspace` — green. Check cargo's own exit code, not a pipeline's; `| tail` reports
  `tail`'s status and will happily show you a passing-looking log for a failed run.
- `cargo clippy --workspace --all-targets --tests -- -D warnings` — clean.
- `poly lint .` — clean. If its `uncomment` lint flags a genuine why-comment, add `~keep` inside the
  comment (every line of a multi-line block) rather than deleting the explanation.
- `cargo test --test max_lines` — no file over the cap.
- `cargo test --test mcp_schema_wire` — no forbidden schema construct; instructions under the ceiling.
- `BASEMIND_HARDEN_NO_BUILD=1 cargo test --release --test harden -- --ignored --nocapture` — 8/8
  green; new canary passes.
