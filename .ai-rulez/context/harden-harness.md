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
2. For each repo: `basemind scan`, then an **in-process git-ops measurement** (`measure_git_ops`):
   build the git-history index synchronously and time warm, microsecond-resolution indexed-vs-live
   latency for `commits_touching` (hot + rare path), `recent_changes`, and `window_commits`, plus the
   build time and on-disk index size. Then it sweeps every MCP code-map + git tool over stdio,
   capturing per-tool latency + result shapes.
3. Asserts canaries (lower bounds, scan-resistant to upstream churn):
   - **tokio**: `find_references("spawn")` returns `>= 200` hits (capped at limit).
   - **django**: `find_references("get")` returns `>= 200` hits.
   - **react**: `search_symbols("useState")` returns `>= 20` hits.
   - **ripgrep-shallow**: `any_truncated == true` (shallow-clone signal surfaces).
   - **every git repo**: the git-history index built (`commits > 0`), and on a repo with real history
     (`>= 1000` commits) indexed `commits_touching` is not slower than the live walk it replaces.

### Knobs

- `BASEMIND_HARDEN_NO_BUILD=1` — skip the release rebuild; reuse `target/release/basemind`. Use this for fast iteration.
- `BASEMIND_HARDEN_FEATURES=<set>` — cargo features to build/run with (default `full`). Set to `""`
  for default features only — needed where the `documents`/`memory`/`intelligence` stack can't
  compile; the harness records those tools as skipped and still measures scan + git-ops.
- `BASEMIND_HARDEN_REPO=<name>` — restrict to a single repo when debugging.
- Per-repo metrics land at `/tmp/basemind-harden-*.log` (full run), the NDJSON results file, and a
  paste-ready git-ops markdown table at `<results-dir>/gitops.md`.

#### Performance baselines

Measured on an Apple M4 (10 cores — 4 P + 6 E, 16 GB), default-feature build (`BASEMIND_HARDEN_FEATURES=""`).

| Repo | Files | Scan time | git-history build | `commits_touching` indexed / live |
|---|---|---|---|---|
| typescript | 81 k | ~18 s | ~3.2 s (2 k commits) | ~37 µs / ~2.3 ms |
| django | 7 k | ~2.4 s | ~0.5 s (2 k commits) | ~39 µs / ~1.6 ms |
| react | 7 k | ~2.0 s | ~0.6 s (2 k commits) | ~39 µs / ~2.5 ms |
| tokio | 861 | ~0.4 s | ~0.9 s (4 k commits) | ~37 µs / ~2.1 ms |

Indexed git queries are ~tens of µs flat; the index is 6–22 % of `.git`. Regressions beyond ~20% on
the scan-time or build-time baselines should be investigated before merge.

#### Canary authoring

See the `harness-canary-authoring` skill. Canaries must be lower bounds (`>=`), call-site-dense,
and stable across repo releases. The `scan_cap = limit * 8` convention bounds work on common names.
Git-ops canaries avoid absolute-time thresholds (machine-dependent) — they assert the index built
and that indexed is not slower than live, gated on a minimum history depth.
