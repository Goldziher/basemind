---
priority: medium
description: "Picking and asserting harden-harness canaries"
---

# Harness Canary Authoring

Use this when adding a new canary to `tests/harden.rs`. Canaries catch real regressions; bad canaries become flaky CI noise.

## Pick a good canary

A canary symbol or query must be:

- **Call-site-dense in the target repo** — at least 50× the threshold. If you assert `>= 200` hits, pick a callee with ≥ 1000 actual call sites in the repo. Headroom absorbs upstream churn.
- **Stable across releases** — pick a fundamental API (`tokio::spawn`, Django's `get`), not a freshly renamed one.
- **Unambiguous by name alone** — gitmind's reference search is name-only. `get` is fine inside Django (many call sites, all the same semantic). Inside React, `get` would match too many unrelated callees; pick `useState` instead.
- **Cheap to scan** — `scan_cap = limit * 8` bounds work, but a hit-dense canary still has the lowest overhead.

## Assertion shape

- Always use lower bounds: `assert!(hits >= N, "expected >= N, got {hits}")`.
- Never assert equality on counts — upstream repo evolution breaks equality assertions silently.
- Capture the canary count in the per-repo metrics struct so regressions are visible in the JSON log even when the assertion passes.

## Steps

1. Pick the target repo + symbol per the criteria above.
2. Confirm the count locally: clone the repo, run `gitmind scan`, call `find_references` via the MCP, note the actual count.
3. Set the threshold to `actual / 2` rounded down — survives ~50% churn.
4. Add to `tests/harden.rs`:
   - The canary call in the per-repo sweep.
   - The assertion next to the existing canaries.
   - The capture into the per-repo metrics struct (mirrors the existing `spawn_hits`, `get_hits`, `useState_hits` fields).
5. Re-run the harness — confirm 8/8 green.

## Pitfalls

- Don't reuse the same canary symbol across repos unless the repo's domain makes it independently meaningful.
- Don't pick a symbol that exists across the standard library — count will explode and the canary becomes uninformative.
- Don't use a symbol that's only in a single file — it's a smoke test, not a canary.
