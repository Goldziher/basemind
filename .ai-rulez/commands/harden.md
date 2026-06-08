---
priority: high
aliases: [h]
usage: "/harden"
description: "Run the real-OSS harden harness against the 8 canary repos"
---

# Harden

Run the harden harness (real-OSS clones + tool sweep + canaries) and summarize.

1. Run:

   ```bash
   BASEMIND_HARDEN_NO_BUILD=1 \
     cargo test --release --test harden -- --ignored --nocapture \
     2>&1 | tee /tmp/basemind-harden-$(date +%s).log
   ```

2. Parse the result via the `harness-interpreter` agent (or inline if trivial).
3. Report:
   - Pass count: `N/8 green` and the names of any failed repos.
   - Canary status: tokio `spawn`, django `get`, react `useState`, ripgrep-shallow truncation.
   - Any per-repo scan-time delta > 20% vs the documented baseline (`typescript ~23 s eager-L2-on`, `tokio < 2 s`, `django < 3 s`).
4. Exit with the harness exit code.

If the user runs `/harden full`, drop `BASEMIND_HARDEN_NO_BUILD=1` and rebuild release first.
