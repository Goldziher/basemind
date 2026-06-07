---
priority: high
aliases: [c]
usage: "/check"
description: "Run the local lint + test triad before committing"
---

# Check

Run the pre-commit triad. Use this before every commit.

1. `cargo fmt`
2. `cargo clippy --workspace --all-targets --tests -- -D warnings`
3. `cargo test --workspace`
4. `prek run -a`

Each step gates the next. On failure:

- `cargo fmt` — re-stage formatted files.
- clippy — fix or justify with a one-line `// allow because…` comment.
- tests — diagnose the failure; never `#[ignore]` to bypass.
- prek — fix per the `prek-and-release-workflow` skill.

After this passes, run `/harden` before pushing if the change touches the scanner / extract / index path.
