#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
destination="${1:?usage: stage-agent-package.sh <empty-directory>}"
mkdir -p "$destination"
[[ -z "$(ls -A "$destination")" ]] || {
  echo "error: destination must be empty" >&2
  exit 1
}
node - "$repo_root" <<'JS'
const fs = require("node:fs");
const path = require("node:path");
const root = process.argv[2];
const manifest = JSON.parse(fs.readFileSync(path.join(root, "opencode-plugin/package.json")));
const source = JSON.parse(fs.readFileSync(path.join(root, "package.json")));
if (manifest.version !== source.version || manifest.private || !manifest.keywords.includes("pi-package")) {
  throw new Error("Pi package metadata must match the root version and be public and discoverable");
}
const skills = fs.readdirSync(path.join(root, "skills"))
  .filter((name) => fs.existsSync(path.join(root, "skills", name, "SKILL.md")));
if (skills.length !== 10) throw new Error(`Expected 10 product skills, found ${skills.length}`);
console.log(`Staging ${skills.length} skills, the Pi extension, and OpenCode adapters`);
JS
cp "$repo_root/opencode-plugin/package.json" "$repo_root/opencode-plugin/README.md" "$repo_root/LICENSE" "$destination/"
cp "$repo_root/opencode-plugin/basemind.js" "$repo_root/opencode-plugin/tui.js" "$destination/"
cp -R "$repo_root/opencode-plugin/commands" "$destination/commands"
mkdir -p "$destination/.pi/extensions"
cp "$repo_root/.pi/extensions/basemind.ts" "$destination/.pi/extensions/"
cp -R "$repo_root/skills" "$destination/skills"
