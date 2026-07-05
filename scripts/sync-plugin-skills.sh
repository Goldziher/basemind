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

# Canonical skills (per-skill SKILL.md) mirrored into every non-Claude tree.
SKILLS=(
	"basemind"
	"basemind-cli"
	"basemind-code-search"
	"basemind-comms"
	"basemind-documents"
	"basemind-doctor"
	"basemind-git-history"
	"basemind-scan"
	"basemind-stats"
)
# Canonical slash commands mirrored into every non-Claude tree.
COMMANDS=(
	"bm"
	"bm-doctor"
	"bm-scan"
	"bm-stats"
)
# Canonical agent-comms hook scripts. These are identical across the harness trees that consume
# `hooks/` (Codex, Cursor) — only each tree's hand-authored `hooks.json` differs (per-harness
# root env var + event contract), so we sync the scripts but NEVER the manifest. Gemini reads the
# repo-root `hooks/` directly (extension root == repo root); opencode uses `basemind.js` instead.
HOOK_SCRIPTS=(
	"session-start"
	"inbox-notify"
	"run-hook.cmd"
)

for skill in "${SKILLS[@]}"; do
	[[ -f "skills/$skill/SKILL.md" ]] || {
		printf 'sync-plugin-skills: missing canonical skill: skills/%s/SKILL.md\n' "$skill" >&2
		exit 1
	}
done
for cmd in "${COMMANDS[@]}"; do
	[[ -f "commands/$cmd.md" ]] || {
		printf 'sync-plugin-skills: missing canonical command: commands/%s.md\n' "$cmd" >&2
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
# Subset of TREES that consume the shared `hooks/` scripts (Codex + Cursor). opencode drives
# comms from `basemind.js`, so it gets no hook-script copies.
HOOK_TREES=(
	".codex-plugin"
	".cursor-plugin"
)

for tree in "${TREES[@]}"; do
	mkdir -p "$tree/commands"
	for skill in "${SKILLS[@]}"; do
		mkdir -p "$tree/skills/$skill"
		cp "skills/$skill/SKILL.md" "$tree/skills/$skill/SKILL.md"
	done
	for cmd in "${COMMANDS[@]}"; do
		cp "commands/$cmd.md" "$tree/commands/$cmd.md"
	done
	printf 'sync-plugin-skills: %s ← skills + commands\n' "$tree"
done

# Mirror the canonical hook scripts (preserving the executable bit) into the hook-consuming
# trees. Each tree keeps its OWN hooks.json (different root env var / event contract).
for tree in "${HOOK_TREES[@]}"; do
	mkdir -p "$tree/hooks"
	for script in "${HOOK_SCRIPTS[@]}"; do
		cp -p "hooks/$script" "$tree/hooks/$script"
	done
	printf 'sync-plugin-skills: %s/hooks ← hook scripts\n' "$tree"
done
