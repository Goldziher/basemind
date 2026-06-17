#!/usr/bin/env bash
# Mirror the canonical SKILL.md files + bm/bm-stats command templates into
# every plugin tree so installed plugins ship the agent-routing docs and the
# `/bm` + `/bm-stats` slash commands.
#
# Idempotent. Run via the prek hook `sync-plugin-skills` or manually:
#   bash scripts/sync-plugin-skills.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

# Canonical sources.
SRC_SKILL_BM="skills/basemind/SKILL.md"
SRC_SKILL_BM_STATS="skills/basemind-stats/SKILL.md"

for src in "$SRC_SKILL_BM" "$SRC_SKILL_BM_STATS"; do
  [[ -f "$src" ]] || {
    printf 'sync-plugin-skills: missing canonical source: %s\n' "$src" >&2
    exit 1
  }
done

# Plugin trees that get the bundle. Each value is the *plugin root* — skills/
# and commands/ live at the plugin root per the Claude Code plugin schema.
TREES=(
  ".claude-plugin"
  ".codex-plugin"
  ".cursor-plugin"
  "opencode-plugin"
)

write_command_bm() {
  local out="$1"
  cat >"$out" <<'EOF'
---
name: bm
description: Ask basemind anything about the current codebase — outlines, refs, callers, git history, blame, diffs, docs, memory.
---

# bm — ask basemind anything about this codebase

Use the basemind MCP server to answer the user's question: $ARGUMENTS

Prefer basemind tools over reading files when navigating large or unfamiliar
codebases. Routing:

- "where is X defined?" → `search_symbols`
- "what calls X?" → `find_references` (any name) or `find_callers` (specific def)
- "shape of this file?" → `outline` (add `l2: true` for calls + docs)
- "what changed recently?" → `recent_changes`, `commits_touching`, `symbol_history`
- "who last touched this?" → `blame_file` / `blame_symbol`
- "where's the churn?" → `hot_files`
- "semantic search across PDFs/docs in the repo?" → `search_documents`
- "recall something the agent remembered earlier?" → `memory_get`
  / `memory_list` / `memory_search`
- "remember this for later sessions?" → `memory_put` (delete with `memory_delete`)
- "refresh the index after editing code?" → `rescan` (or `rescan { paths: [...] }`
  to limit to changed files)
EOF
}

write_command_bm_stats() {
  local out="$1"
  cat >"$out" <<'EOF'
---
name: bm-stats
description: Show basemind activity dashboard — tool calls, per-tool histogram, estimated tokens saved.
---

# bm-stats — basemind activity dashboard

Call the basemind MCP tool `telemetry_summary` and render the result as a
markdown dashboard.

Default window is `today`. If the user asks for a specific range, map it to one
of `today`, `1h`, `24h`, `all`.

Always end with a one-sentence disclosure that the savings number is heuristic:
tools without a realistic baseline (memory, document search, git wrappers)
report 0 saved.
EOF
}

for tree in "${TREES[@]}"; do
  mkdir -p "$tree/skills/basemind" "$tree/skills/basemind-stats" "$tree/commands"
  cp "$SRC_SKILL_BM" "$tree/skills/basemind/SKILL.md"
  cp "$SRC_SKILL_BM_STATS" "$tree/skills/basemind-stats/SKILL.md"
  write_command_bm "$tree/commands/bm.md"
  write_command_bm_stats "$tree/commands/bm-stats.md"
  printf 'sync-plugin-skills: %s ← skills + commands\n' "$tree"
done
