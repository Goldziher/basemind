#!/usr/bin/env bash
# Mirror the canonical skills + slash commands into every NON-Claude plugin tree.
#
# Layout model (per each harness's plugin schema):
#   - Claude Code: the plugin root IS the repo root (marketplace.json `source: "./"`),
#     so its components live at the repo root — `skills/` and `commands/` — and the
#     `.claude-plugin/` directory holds ONLY the manifests (plugin.json,
#     marketplace.json) + statusline.sh. Claude IGNORES component dirs nested inside
#     `.claude-plugin/`. We therefore treat the repo-root `skills/` + `commands/` as
#     canonical and do NOT copy anything into `.claude-plugin/`.
#   - Codex / Cursor / OpenCode: each `<harness>-plugin/` directory is itself the
#     plugin root, so those trees get their own `skills/` + `commands/` copies.
#
# Idempotent. Run via the prek hook `sync-plugin-skills` or manually:
#   bash scripts/sync-plugin-skills.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

# Canonical sources — these double as the Claude plugin's components at the repo root.
CANONICAL=(
  "skills/basemind/SKILL.md"
  "skills/basemind-cli/SKILL.md"
  "skills/basemind-stats/SKILL.md"
  "commands/bm.md"
  "commands/bm-stats.md"
)

for src in "${CANONICAL[@]}"; do
  [[ -f "$src" ]] || {
    printf 'sync-plugin-skills: missing canonical source: %s\n' "$src" >&2
    exit 1
  }
done

# Plugin trees whose root is their own directory (NOT the repo root). Claude is
# excluded on purpose — it consumes the repo-root skills/ + commands/ directly.
TREES=(
  ".codex-plugin"
  ".cursor-plugin"
  "opencode-plugin"
)

for tree in "${TREES[@]}"; do
  mkdir -p "$tree/skills/basemind" "$tree/skills/basemind-cli" "$tree/skills/basemind-stats" "$tree/commands"
  cp "skills/basemind/SKILL.md" "$tree/skills/basemind/SKILL.md"
  cp "skills/basemind-cli/SKILL.md" "$tree/skills/basemind-cli/SKILL.md"
  cp "skills/basemind-stats/SKILL.md" "$tree/skills/basemind-stats/SKILL.md"
  cp "commands/bm.md" "$tree/commands/bm.md"
  cp "commands/bm-stats.md" "$tree/commands/bm-stats.md"
  printf 'sync-plugin-skills: %s ← skills + commands\n' "$tree"
done
