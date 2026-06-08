---
priority: high
---

# Harden Harness

`tests/harden.rs` is the real-OSS canary harness. `#[ignore]`-gated; run with:

```bash
cargo test --release --test harden -- --ignored --nocapture
```

## What it does

1. Clones (or refreshes) 8 real OSS repos under `/tmp/basemind-harden/`:

   `ripgrep` (Rust), `tokio` (Rust), `typescript` (TS/JS), `react` (TS/JSX), `django` (Python),
   `requests` (Python), `gin` (Go), `ripgrep-shallow` (shallow clone smoke).
2. For each repo: `basemind scan` then sweeps every MCP code-map tool plus a representative subset
   of the git tools, capturing per-tool latency + result shapes.
3. Asserts canaries (lower bounds, scan-resistant to upstream churn):
   - **tokio**: `find_references("spawn")` returns `>= 200` hits (capped at limit).
   - **django**: `find_references("get")` returns `>= 200` hits.
   - **react**: `search_symbols("useState")` returns `>= 20` hits.
   - **ripgrep-shallow**: `any_truncated == true` (shallow-clone signal surfaces).

### Knobs

- `BASEMIND_HARDEN_NO_BUILD=1` — skip the release rebuild; reuse `target/release/basemind`. Use this for fast iteration.
- `BASEMIND_HARDEN_REPO=<name>` — restrict to a single repo when debugging.
- Per-repo metrics land at `/tmp/basemind-harden-*.log` (full run) and a metrics JSON next to it.

#### Performance baselines

| Repo | Files | Scan time | Notes |
|---|---|---|---|
| typescript | ~39 k | 13.5 s (eager L2 off) / ~23 s (eager L2 on) | Largest in the harness. |
| tokio | ~2 k | < 2 s | Spawn-call canary. |
| django | ~3 k | < 3 s | Get-call canary. |

Regressions beyond ~20% on these baselines should be investigated before merge.

#### Canary authoring

See the `harness-canary-authoring` skill. Canaries must be lower bounds (`>=`), call-site-dense,
and stable across repo releases. The `scan_cap = limit * 8` convention bounds work on common names.
