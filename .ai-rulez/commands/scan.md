---
priority: high
aliases: [s]
usage: "/scan [path]"
description: "Run basemind scan against the current working tree (or a path argument)"
---

# Scan

Run a full `basemind scan` and report the result.

1. Build if stale: `cargo build --release` (skip if `target/release/basemind` is fresh).
2. Run:

   ```bash
   ./target/release/basemind scan ${1:-.}
   ```

3. Report from stdout / the resulting `.basemind/`:
   - Wall-clock scan time.
   - Total files indexed.
   - Total symbols extracted.
   - Whether eager L2 was active (check `.ai-rulez`-equivalent config: `eager_l2` in `basemind.toml` or default `true`).
4. If scan time exceeds the baseline for the repo size by > 20%, flag the regression.

This command exists to drive perf iteration loops — pair it with `/harden` to confirm no regression on the canary repos.
