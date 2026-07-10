#!/usr/bin/env bash

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

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
COMMANDS=(
	"bm"
	"bm-doctor"
	"bm-init"
	"bm-scan"
	"bm-statusline"
	"bm-stats"
)
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

TREES=(
	".codex-plugin"
	".cursor-plugin"
	"opencode-plugin"
	"pip-package/basemind"
)
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

for tree in "${HOOK_TREES[@]}"; do
	mkdir -p "$tree/hooks"
	for script in "${HOOK_SCRIPTS[@]}"; do
		cp -p "hooks/$script" "$tree/hooks/$script"
	done
	printf 'sync-plugin-skills: %s/hooks ← hook scripts\n' "$tree"
done
