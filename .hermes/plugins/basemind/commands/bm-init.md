---
name: bm-init
description: Onboard (or refresh) basemind in this repo — write basemind.toml, gitignore the cache, and inject a "prefer basemind over grep/read/git" rules block into CLAUDE.md / AGENTS.md / ai-rulez.
argument-hint: [capabilities…]
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:9709ae46a4fe2ddf3fb12642716bbd876d0cd7a9dc82ecc8d5a7b4efadf09531
Source-Hash: blake3:ca37d0bd1accfd4a3156158a1f47fd4568c00994d83f95330f41124a857f34dc
Schema-Version: v1
-->

# bm-init — onboard basemind into this repo

Run `basemind init` so the repo has a committed `basemind.toml`, a gitignored `.basemind/` cache,
and a rules block that tells every agent to prefer basemind's MCP tools over grep, file reads, and
naked `git`. CLI and slash command share ONE implementation — this just drives `basemind init`
with the right non-interactive flags.

## When to use

First time setting up basemind in a repo, or to refresh the rules block after enabling new
capabilities (documents/RAG, agent-comms, semantic search). Safe to re-run: it's idempotent.

## How to use

1. **Ask which capabilities matter** (one short question). The options are:
   `code-search-navigation`, `code-mapping-architecture`, `git-history`, `agent-comms`,
   `documents-rag`, `semantic-search`. If the user has no preference, enable all.

2. **Run `basemind init` non-interactively** with the matching flags. Enable everything:

   ```sh
   basemind init --yes
   ```

   Narrow to a subset with repeatable `--with` (allow-list) or `--without` (subtract):

   ```sh
   basemind init --yes --with code-search-navigation --with git-history
   ```

   Steer where the rules land with `--rules-target <auto|claude|agents|ai-rulez|none>` (default
   `auto`). Preview without writing using `--print`.

3. **Report what changed** — which files were written or kept (`basemind.toml`, `.gitignore`, the
   rules file), and whether the delimited block was created or updated in place.

## Notes

- Source-of-truth detection (auto): `.ai-rulez/config.toml` present → writes
  `.ai-rulez/rules/basemind-usage.md` (then tell the user to run `ai-rulez generate`; do NOT run
  it for them). Else CLAUDE.md → AGENTS.md → create CLAUDE.md, wrapping the content in an
  idempotent `<!-- BEGIN basemind … -->` … `<!-- END basemind -->` block that is replaced in
  place on re-run, never duplicated. Content outside the markers is never touched.
- An existing `basemind.toml` is kept verbatim, never clobbered.
- If `basemind` isn't on `PATH`: use the plugin-managed cache binary or build a dev binary with
  `cargo build --release` and use `./target/release/basemind`.

## See also

The `bm-scan` command to build the index next, and the `basemind` skill for the full MCP tool
surface the rules block advertises.
