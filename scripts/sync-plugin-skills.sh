#!/usr/bin/env bash

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

# Derived from the canonical trees rather than listed, so adding a skill or command ships it
# everywhere instead of silently reaching only the trees someone remembered to update. The
# hand-maintained list had drifted to 9 of 10 skills: multi-agent-room existed in skills/ and was
# referenced by the shipped agent-comms rule, but was absent from every plugin bundle that rule
# ships into, so the agent was told to consult a skill its host did not have. ~keep
SKILLS=()
while IFS= read -r dir; do
	SKILLS+=("$(basename "$dir")")
done < <(find skills -mindepth 1 -maxdepth 1 -type d | sort)
COMMANDS=()
while IFS= read -r file; do
	COMMANDS+=("$(basename "$file" .md)")
done < <(find commands -mindepth 1 -maxdepth 1 -name '*.md' | sort)

[[ ${#SKILLS[@]} -gt 0 && ${#COMMANDS[@]} -gt 0 ]] || {
	printf 'sync-plugin-skills: no canonical skills or commands found — wrong cwd?\n' >&2
	exit 1
}
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

AI_RULEZ="${AI_RULEZ:-ai-rulez}"
command -v "$AI_RULEZ" >/dev/null 2>&1 || {
	printf 'sync-plugin-skills: ai-rulez is required to generate Hermes packages\n' >&2
	exit 1
}
"$AI_RULEZ" generate --plugin

for tree in "${HOOK_TREES[@]}"; do
	mkdir -p "$tree/hooks"
	for script in "${HOOK_SCRIPTS[@]}"; do
		cp -p "hooks/$script" "$tree/hooks/$script"
	done
	printf 'sync-plugin-skills: %s/hooks ← hook scripts\n' "$tree"
done
