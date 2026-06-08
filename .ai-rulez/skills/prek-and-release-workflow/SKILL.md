---
priority: medium
description: "Pre-commit lint/test/harness order"
---

# Prek + Release Workflow

The canonical order to run before pushing a change. Each step gates the next; do not skip.

## Local check loop

```bash
cargo fmt
cargo clippy --workspace --all-targets --tests -- -D warnings
cargo test --workspace
prek run -a
```

Why this order:

- `cargo fmt` first so clippy / prek don't fail on formatting.
- Clippy strict (`-D warnings`) — surface real issues before the broader prek sweep.
- Unit + integration tests catch logic regressions before the slow harness.
- `prek run -a` is the meta-linter: typos, markdownlint (120-char cap), cargo-deny licenses, cargo-machete unused deps, rustdoc-lint, rust-max-lines (1000-line cap on `src/**/*.rs`).

## Harden harness

After the local loop passes:

```bash
BASEMIND_HARDEN_NO_BUILD=1 \
  cargo test --release --test harden -- --ignored --nocapture \
  2>&1 | tee /tmp/basemind-harden-$(date +%s).log
```

- `BASEMIND_HARDEN_NO_BUILD=1` reuses `target/release/basemind` — saves ~30s per run. Drop it when you're suspicious the binary is stale.
- Expect 8/8 green: ripgrep, tokio, typescript, react, django, requests, gin, ripgrep-shallow.
- Canaries: tokio `spawn_hits >= 200`, django `get_hits >= 200`, react `useState_hits >= 20`, ripgrep-shallow `any_truncated == true`.

## Commit + push

- Conventional Commit prefix (`feat:`, `fix:`, `perf:`, `chore:`, `refactor:`).
- Body explains *why*. Mention schema bumps (`INDEX_SCHEMA_VER` / blob format) and added dependencies.
- Co-author trailer per repo convention.
- Push directly to `main` is approved for the active iteration loop; PRs only when explicitly requested.

## When something fails

- typos: fix the misspelling; prefer the dictionary correction unless it's a proper noun.
- markdownlint MD013 (line length > 120): split the line; for table cells, shorten the cell or move details to a code block under the table.
- cargo-deny license rejection: add the SPDX expression to `deny.toml`'s `licenses.allow` only if the license is genuinely permissive (Apache, MIT family, ISC, 0BSD, BSL, Unicode). Anything copyleft (GPL family) — escalate.
- rust-max-lines: extract a submodule or helper file. Do not raise the cap.
