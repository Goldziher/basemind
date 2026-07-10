#!/usr/bin/env bash
# Atomically bump the basemind version across every shipped surface.
# Usage: ./scripts/release-bump.sh <version>
#   Cargo.toml                            [package] version
#   npm-package/package.json              "version"

set -euo pipefail

VERSION="${1:?usage: release-bump.sh <version>}"

if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?$ ]]; then
	echo "error: version must be MAJOR.MINOR.PATCH or MAJOR.MINOR.PATCH-rc.N (got '$VERSION')" >&2
	exit 1
fi

PY_VERSION="${VERSION//-rc./rc}"

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

MAJOR="$(echo "$VERSION" | cut -d. -f1)"
MINOR="$(echo "$VERSION" | cut -d. -f2)"
RELEASE_MINOR=$((MAJOR * 100 + MINOR))

if [[ "$MAJOR" == "0" ]]; then
	RELEASE_MINOR="$MINOR"
fi

CURRENT_RELEASE_MINOR="$(grep -E 'pub const RELEASE_MINOR' src/version.rs | sed -E 's/.* = ([0-9]+);.*/\1/')"

echo "→ Cargo.toml         → $VERSION"
sed -i.bak -E "s/^version = \"[^\"]+\"$/version = \"$VERSION\"/" Cargo.toml
rm Cargo.toml.bak

if [[ -f npm-package/package.json ]]; then
	echo "→ npm-package        → $VERSION"
	sed -i.bak -E "s/\"version\": \"[^\"]+\"/\"version\": \"$VERSION\"/" npm-package/package.json
	rm npm-package/package.json.bak
fi

if [[ -f pip-package/pyproject.toml ]]; then
	echo "→ pip-package        → $PY_VERSION"
	sed -i.bak -E "s/^version = \"[^\"]+\"$/version = \"$PY_VERSION\"/" pip-package/pyproject.toml
	rm pip-package/pyproject.toml.bak
fi

if [[ -f pip-package/basemind/__init__.py ]]; then
	sed -i.bak -E "s/^__version__ = \"[^\"]+\"$/__version__ = \"$PY_VERSION\"/" pip-package/basemind/__init__.py
	rm pip-package/basemind/__init__.py.bak
fi

if [[ -f pip-package/basemind/plugin.yaml ]]; then
	echo "→ pip-package/basemind/plugin.yaml → $VERSION"
	sed -i.bak -E "s/^version: .*/version: \"$VERSION\"/" pip-package/basemind/plugin.yaml
	rm pip-package/basemind/plugin.yaml.bak
fi

bump_json_version() {
	local file="$1"
	[[ -f "$file" ]] || return 0
	echo "→ ${file}        → $VERSION"
	sed -i.bak -E "s/(\"version\"[[:space:]]*:[[:space:]]*\")[^\"]+(\")/\1${VERSION}\2/" "$file"
	rm "${file}.bak"
}

bump_json_version package.json
bump_json_version opencode-plugin/package.json
bump_json_version .claude-plugin/plugin.json
bump_json_version .codex-plugin/plugin.json
bump_json_version .cursor-plugin/plugin.json
bump_json_version gemini-extension.json
bump_json_version kimi.plugin.json
bump_json_version .claude-plugin/marketplace.json
bump_json_version .agents/plugins/marketplace.json

if [[ "$CURRENT_RELEASE_MINOR" != "$RELEASE_MINOR" ]]; then
	echo "→ RELEASE_MINOR      $CURRENT_RELEASE_MINOR → $RELEASE_MINOR (minor bump — schema wipe on next scan)"
	sed -i.bak -E "s/^pub const RELEASE_MINOR: u16 = [0-9]+;/pub const RELEASE_MINOR: u16 = $RELEASE_MINOR;/" src/version.rs
	rm src/version.rs.bak
else
	echo "→ RELEASE_MINOR      $CURRENT_RELEASE_MINOR (no change — patch bump)"
fi

cargo build --quiet 2>/dev/null || true

echo
echo "Validating version consistency across all surfaces..."
validation_failed=0

cargo_version="$(grep -E '^version = "' Cargo.toml | head -1 | cut -d'"' -f2)"
if [ "$cargo_version" != "$VERSION" ]; then
	echo "✗ Cargo.toml: expected $VERSION, got $cargo_version"
	validation_failed=1
fi

if [ -f npm-package/package.json ]; then
	npm_version="$(jq -r '.version' npm-package/package.json 2>/dev/null || echo '')"
	if [ "$npm_version" != "$VERSION" ]; then
		echo "✗ npm-package/package.json: expected $VERSION, got $npm_version"
		validation_failed=1
	fi
fi

if [ -f pip-package/pyproject.toml ]; then
	pypi_version="$(grep -E '^version = "' pip-package/pyproject.toml | head -1 | cut -d'"' -f2)"
	if [ "$pypi_version" != "$PY_VERSION" ]; then
		echo "✗ pip-package/pyproject.toml: expected $PY_VERSION, got $pypi_version"
		validation_failed=1
	fi
fi

if [ -f pip-package/basemind/__init__.py ]; then
	init_version="$(grep -E '^__version__ = "' pip-package/basemind/__init__.py | cut -d'"' -f2)"
	if [ "$init_version" != "$PY_VERSION" ]; then
		echo "✗ pip-package/basemind/__init__.py: expected $PY_VERSION, got $init_version"
		validation_failed=1
	fi
fi

if [ -f pip-package/basemind/plugin.yaml ]; then
	hermes_version="$(grep -E '^version: ' pip-package/basemind/plugin.yaml | head -1 | sed -E 's/^version: "?([^"]+)"?/\1/')"
	if [ "$hermes_version" != "$VERSION" ]; then
		echo "✗ pip-package/basemind/plugin.yaml: expected $VERSION, got $hermes_version"
		validation_failed=1
	fi
fi

if [ -f package.json ]; then
	root_version="$(jq -r '.version' package.json 2>/dev/null || echo '')"
	if [ "$root_version" != "$VERSION" ]; then
		echo "✗ package.json (root): expected $VERSION, got $root_version"
		validation_failed=1
	fi
fi

if [ -f opencode-plugin/package.json ]; then
	opencode_version="$(jq -r '.version' opencode-plugin/package.json 2>/dev/null || echo '')"
	if [ "$opencode_version" != "$VERSION" ]; then
		echo "✗ opencode-plugin/package.json: expected $VERSION, got $opencode_version"
		validation_failed=1
	fi
fi

if [ -f .claude-plugin/plugin.json ]; then
	claude_version="$(jq -r '.version' .claude-plugin/plugin.json 2>/dev/null || echo '')"
	if [ "$claude_version" != "$VERSION" ]; then
		echo "✗ .claude-plugin/plugin.json: expected $VERSION, got $claude_version"
		validation_failed=1
	fi
fi

if [ -f .claude-plugin/marketplace.json ]; then
	marketplace_version="$(jq -r '.plugins[0].version' .claude-plugin/marketplace.json 2>/dev/null || echo '')"
	if [ "$marketplace_version" != "$VERSION" ]; then
		echo "✗ .claude-plugin/marketplace.json: expected $VERSION, got $marketplace_version"
		validation_failed=1
	fi
fi

if [ -f .agents/plugins/marketplace.json ]; then
	codex_marketplace_version="$(jq -r '.plugins[0].version' .agents/plugins/marketplace.json 2>/dev/null || echo '')"
	if [ "$codex_marketplace_version" != "$VERSION" ]; then
		echo "✗ .agents/plugins/marketplace.json: expected $VERSION, got $codex_marketplace_version"
		validation_failed=1
	fi
fi

if [ -f .codex-plugin/plugin.json ]; then
	codex_version="$(jq -r '.version' .codex-plugin/plugin.json 2>/dev/null || echo '')"
	if [ "$codex_version" != "$VERSION" ]; then
		echo "✗ .codex-plugin/plugin.json: expected $VERSION, got $codex_version"
		validation_failed=1
	fi
fi

if [ -f .cursor-plugin/plugin.json ]; then
	cursor_version="$(jq -r '.version' .cursor-plugin/plugin.json 2>/dev/null || echo '')"
	if [ "$cursor_version" != "$VERSION" ]; then
		echo "✗ .cursor-plugin/plugin.json: expected $VERSION, got $cursor_version"
		validation_failed=1
	fi
fi

if [ -f gemini-extension.json ]; then
	gemini_version="$(jq -r '.version' gemini-extension.json 2>/dev/null || echo '')"
	if [ "$gemini_version" != "$VERSION" ]; then
		echo "✗ gemini-extension.json: expected $VERSION, got $gemini_version"
		validation_failed=1
	fi
fi

if [ -f kimi.plugin.json ]; then
	kimi_version="$(jq -r '.version' kimi.plugin.json 2>/dev/null || echo '')"
	if [ "$kimi_version" != "$VERSION" ]; then
		echo "✗ kimi.plugin.json: expected $VERSION, got $kimi_version"
		validation_failed=1
	fi
fi

if [ $validation_failed -eq 0 ]; then
	echo "✓ All version surfaces are consistent: $VERSION"
else
	echo "error: version validation failed. Review the above and fix manually." >&2
	exit 1
fi

echo
echo "Done. Review with: git diff"
echo "Next: cargo test --workspace && git commit -am 'chore(release): v$VERSION'"
