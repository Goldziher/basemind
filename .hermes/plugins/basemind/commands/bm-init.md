---
name: bm-init
description: Onboard (or refresh) basemind in this repo — write basemind.toml and inject a "prefer basemind over grep/read/git" rules block into a rules file you choose (CLAUDE.local.md / AGENTS.local.md / CLAUDE.md / AGENTS.md / ai-rulez).
argument-hint: [capabilities…]
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:ce5cc979d890245adcc54873c43cac7c1e49c28763b4af94cb60ad2b0d37a108
Source-Hash: blake3:8e3d03b0a29d70e7684fb65ed1186f800ac703498c14f053341d8869d89b2bbe
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

   Exception: if `.ai-rulez/config.toml` is present, ai-rulez owns governance — write the
   gitignored `.ai-rulez/local/rules/basemind-usage.md` instead of asking (never the committed
   `.ai-rulez/rules/basemind-usage.md`, and never CLAUDE.md/AGENTS.md).

3. **Ask about auto-approving basemind's MCP tools (one short question).** basemind can add one
   `permissions.allow` glob entry so Claude Code stops prompting for approval on its own tools.
   Present the choice:
   - `.claude/settings.local.json` — personal, gitignored (**recommended default**)
   - `.claude/settings.json` — committed, shared with everyone on the repo
   - `none` — skip this step

4. **Run `basemind init` non-interactively** with the matching flags. Pass the user's rules choice
   via `--rules-target <claude-local|agents-local|claude|agents|ai-rulez-local|ai-rulez|none>`
   (default `auto` resolves to the gitignored local file, never a committed one) and their
   settings choice via `--settings-target <local|shared|none>` (omitting the flag is equivalent to
   `none` in this non-interactive flow — it is never added silently):

   ```sh
   basemind init --yes --rules-target claude-local --settings-target local
   ```

   Narrow capabilities with repeatable `--with` (allow-list) or `--without` (subtract):

   ```sh
   basemind init --yes --rules-target claude-local --settings-target local --with code-search-navigation --with git-history
   ```

   Preview without writing using `--print`.

5. **Report what changed** — which files were written or kept (`basemind.toml`, the chosen rules
   file, the settings file if opted into), whether the delimited block was created or updated in
   place, and any `.gitignore` pattern that was added (see below).

## Notes

- `--rules-target auto` (the default) NEVER writes a committed `CLAUDE.md` / `AGENTS.md` unasked:
  it routes to the gitignored `.ai-rulez/local/rules/basemind-usage.md` when `.ai-rulez/config.toml`
  owns governance (then tell the user to run `ai-rulez generate`; do NOT run it for them), otherwise
  to the gitignored `CLAUDE.local.md` (or `AGENTS.local.md` when the repo uses AGENTS). Choosing
  `claude` / `agents` / `ai-rulez` explicitly opts into the corresponding committed file.
- The block is wrapped in an idempotent `<!-- BEGIN basemind … -->` … `<!-- END basemind -->`
  block that is replaced in place on re-run, never duplicated. Content outside the markers is
  never touched.
- **`.gitignore` coverage**: whenever `init` is about to write one of the gitignored-by-convention
  files (`CLAUDE.local.md`, `AGENTS.local.md`, `.claude/settings.local.json`, anything under
  `.ai-rulez/local/`), it checks whether an existing `.gitignore` already covers that path. If not,
  it prompts `add "<pattern>" to .gitignore? [Y/n]` interactively, or appends the pattern
  automatically with `--yes`, so a personal `.local` file is never one accidental `git add -A` away
  from being committed.
- The settings-permissions step is opt-in and defaults to **skipped** non-interactively — pass
  `--settings-target local` (or `shared`) explicitly to enable it. The merge only ever
  adds/dedupes basemind's own entry inside `permissions.allow`; every other key in
  `.claude/settings*.json` (e.g. `skillOverrides`), and its position in the file, is left exactly
  as it was.
- An existing `basemind.toml` is kept verbatim, never clobbered.
- If `basemind` isn't on `PATH`: use the plugin-managed cache binary or build a dev binary with
  `cargo build --release` and use `./target/release/basemind`.

## See also

The `bm-scan` command to build the index next, and the `basemind` skill for the full MCP tool
surface the rules block advertises.
