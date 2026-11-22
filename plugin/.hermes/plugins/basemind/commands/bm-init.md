---
description: Onboard (or refresh) basemind in this repo — write basemind.toml and inject a "prefer basemind over grep/read/git" rules block into a rules file you choose (CLAUDE.local.md / AGENTS.local.md / CLAUDE.md / AGENTS.md / ai-rulez).
argument-hint:
    - capabilities…
name: bm-init
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:e6893f12949b02628f316507ca77686ddc810f736293464903bf242c632c0631
Source-Hash: blake3:fe6b745cadf09d25b5dd65b3ce73953d67a40a56509cf6f7797a155314abfc3a
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
   `code-search-navigation`, `code-mapping-architecture`, `git-history`, `file-finding`,
   `agent-comms`, `documents-rag`, `semantic-search`. If the user has no preference, enable all.

2. **Ask where the rules block should go — do NOT assume.** Never write a committed `CLAUDE.md`
   or `AGENTS.md` without the user's explicit say-so. Present the choice:
   - `CLAUDE.local.md` — personal, gitignored (**recommended default**)
   - `AGENTS.local.md` — personal, gitignored; no agent host loads it by itself (OpenCode does once
     `"instructions": ["AGENTS.local.md"]` is in `~/.config/opencode/opencode.json`)
   - `CLAUDE.md` — committed, shared with everyone on the repo
   - `AGENTS.md` — committed, shared with everyone on the repo
   - `none` — write no rules

   Exception: if `.ai-rulez/config.toml` is present, ai-rulez owns governance — write the
   gitignored `.ai-rulez/local/rules/basemind-usage.md` instead of asking (never the committed
   `.ai-rulez/rules/basemind-usage.md`, and never CLAUDE.md/AGENTS.md).

3. **Ask about auto-approving basemind's read-only MCP tools (one short question).** basemind can
   add one `permissions.allow` entry per read-only tool so Claude Code stops prompting for them;
   `memory_save`, `index_rescan` and the agent tools still ask. Present the choice:
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

   Preview without writing using `--print`. Without `--yes` and without a terminal, init writes
   nothing and only prints what it would change, so ask the user before passing `--yes`.

5. **Report what changed** — which files were written or kept (`basemind.toml`, the chosen rules
   file, the settings file if opted into), whether the delimited block was created or updated in
   place, and any `.gitignore` pattern that was added (see below).

## Notes

- `--rules-target auto` (the default) NEVER writes a committed `CLAUDE.md` / `AGENTS.md` unasked:
  it routes to the gitignored `.ai-rulez/local/rules/basemind-usage.md` when `.ai-rulez/config.toml`
  owns governance (then tell the user to run `ai-rulez generate`; do NOT run it for them), otherwise
  to the gitignored `CLAUDE.local.md` (or `AGENTS.local.md` when the repo uses AGENTS; pass on the
  note `init` prints about which hosts read it). Choosing
  `claude` / `agents` / `ai-rulez` explicitly opts into the corresponding committed file.
- The block sits between HTML-comment markers (`BEGIN basemind` … `END basemind`) and is
  replaced in place on re-run, never duplicated. Content outside the markers is never touched.
- **`.gitignore` coverage**: whenever `init` is about to write one of the files that are
  gitignored by convention (`CLAUDE.local.md`, `AGENTS.local.md`, `.claude/settings.local.json`,
  anything under `.ai-rulez/local/`), it checks whether an existing `.gitignore` already covers
  that path. If not, it prompts `add "<pattern>" to .gitignore? [Y/n]` interactively, or appends
  the pattern automatically with `--yes`, so a personal `.local` file is never one accidental
  `git add -A` away from being committed.
- The settings-permissions step is opt-in and defaults to **skipped** non-interactively — pass
  `--settings-target local` (or `shared`) explicitly to enable it. The merge adds each
  read-only tool once to `permissions.allow` and removes only the `mcp__plugin_basemind_basemind__*`
  wildcard older releases wrote and entries for retired tools (`__code`, `__git`, …); every other key in
  `.claude/settings*.json` (e.g. `skillOverrides`), and its position in the file, is left exactly
  as it was.
- An existing `basemind.toml` is kept verbatim, never clobbered.
- Prefer the plugin-managed, version-matched binary: `${CLAUDE_PLUGIN_ROOT}/scripts/mcp-launch.sh init …`
  (in other hosts, `scripts/mcp-launch.sh` in the plugin directory). Use bare `basemind` from `PATH`
  only when no plugin launcher exists, or build a dev binary with `cargo build --release` and use
  `./target/release/basemind`.

## See also

The `bm-scan` command to build the index next, and the `basemind` skill for the full MCP tool
surface the rules block advertises.
