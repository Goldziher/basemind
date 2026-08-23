---
name: bm-init
description: Onboard (or refresh) basemind in this repo — write basemind.toml and inject a "prefer basemind over grep/read/git" rules block into a rules file you choose (CLAUDE.local.md / AGENTS.local.md / CLAUDE.md / AGENTS.md / ai-rulez).
argument-hint: [capabilities…]
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:e5682e8a1c3a00e1d15399c8b7d16a1cb2ce36f5c541da9c84b2104eeb4a6794
Source-Hash: blake3:b186a580d8f0dbf759865f1f682e7fbdb6a0a4d5beebff0985f869e6f4f63acc
Schema-Version: v1
-->

# bm-init — onboard basemind into this repo

Run `basemind init` so the repo has a committed `basemind.toml` and a rules block that tells every
agent to prefer basemind's MCP tools over grep, file reads, and naked `git`. CLI and slash command
share ONE implementation — this just drives `basemind init` with the right non-interactive flags.

## When to use

First time setting up basemind in a repo, or to refresh the rules block after enabling new
capabilities (documents/RAG, agent-comms, semantic search). Safe to re-run: it's idempotent.

## How to use

1. **Ask which capabilities matter** (one short question). The options are:
   `code-search-navigation`, `code-mapping-architecture`, `git-history`, `agent-comms`,
   `documents-rag`, `semantic-search`. If the user has no preference, enable all.

2. **Ask where the rules block should go — do NOT assume.** Never write a committed `CLAUDE.md`
   or `AGENTS.md` without the user's explicit say-so. Present the choice:
   - `CLAUDE.local.md` — personal, gitignored (**recommended default**)
   - `AGENTS.local.md` — personal, gitignored (for AGENTS-based tools)
   - `CLAUDE.md` — committed, shared with everyone on the repo
   - `AGENTS.md` — committed, shared with everyone on the repo
   - `none` — write no rules

   Exception: if `.ai-rulez/config.toml` is present, ai-rulez owns governance — write the ai-rulez
   rule file instead of asking (it never touches CLAUDE.md/AGENTS.md).

3. **Run `basemind init` non-interactively** with the matching flags. Pass the user's rules choice
   via `--rules-target <claude-local|agents-local|claude|agents|ai-rulez|none>` (default `auto`
   resolves to the gitignored local file, never a committed one):

   ```sh
   basemind init --yes --rules-target claude-local
   ```

   Narrow capabilities with repeatable `--with` (allow-list) or `--without` (subtract):

   ```sh
   basemind init --yes --rules-target claude-local --with code-search-navigation --with git-history
   ```

   Preview without writing using `--print`.

4. **Report what changed** — which files were written or kept (`basemind.toml`, the chosen rules
   file), and whether the delimited block was created or updated in place.

## Notes

- `--rules-target auto` (the default) NEVER writes a committed `CLAUDE.md` / `AGENTS.md` unasked:
  it routes to `.ai-rulez/rules/basemind-usage.md` when `.ai-rulez/config.toml` owns governance
  (then tell the user to run `ai-rulez generate`; do NOT run it for them), otherwise to the
  gitignored `CLAUDE.local.md` (or `AGENTS.local.md` when the repo uses AGENTS). Choosing `claude`
  / `agents` explicitly opts into the committed file.
- The block is wrapped in an idempotent `<!-- BEGIN basemind … -->` … `<!-- END basemind -->`
  block that is replaced in place on re-run, never duplicated. Content outside the markers is
  never touched.
- An existing `basemind.toml` is kept verbatim, never clobbered.
- If `basemind` isn't on `PATH`: use the plugin-managed cache binary or build a dev binary with
  `cargo build --release` and use `./target/release/basemind`.

## See also

The `bm-scan` command to build the index next, and the `basemind` skill for the full MCP tool
surface the rules block advertises.
