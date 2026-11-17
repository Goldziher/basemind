#!/usr/bin/env bash
# Atomically bump the basemind version across every shipped surface.
# Usage: ./scripts/release-bump.sh <version>
#   Cargo.toml                            [package] version, and its basemind-agents requirement
#   crates/basemind-agents/Cargo.toml     [package] version (published beside basemind, lock-step)
#   npm-package/package.json              "version"

set -euo pipefail

VERSION="${1:?usage: release-bump.sh <version>}"

if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?$ ]]; then
  echo "error: version must be MAJOR.MINOR.PATCH or MAJOR.MINOR.PATCH-rc.N (got '$VERSION')" >&2
  exit 1
fi

PY_VERSION="${VERSION//-rc./rc}"

# Checked before any file is rewritten, so a missing ai-rulez cannot leave a half-bumped tree. ~keep
AI_RULEZ="${AI_RULEZ:-ai-rulez}"
command -v "$AI_RULEZ" >/dev/null 2>&1 || {
  echo "error: ai-rulez is required to regenerate the Hermes package" >&2
  exit 1
}

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
sed -i.bak -E "s/^(basemind-agents = \{ version = \")[^\"]+(\")/\1$VERSION\2/" Cargo.toml
rm Cargo.toml.bak

# basemind-agents is published to crates.io with basemind and moves with it (ADR-0040).
echo "→ basemind-agents    → $VERSION"
sed -i.bak -E "s/^version = \"[^\"]+\"$/version = \"$VERSION\"/" crates/basemind-agents/Cargo.toml
rm crates/basemind-agents/Cargo.toml.bak

# The config schema snapshot embeds the crate version in the default user agent;
# tests/config_schema.rs asserts it byte-equal, so it must move with the version.
echo "→ config schema      → $VERSION"
sed -i.bak -E "s#basemind/[0-9][^ ]* \(\+https#basemind/$VERSION (+https#" schema/basemind-config-v1.schema.json
rm schema/basemind-config-v1.schema.json.bak

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

echo "→ Hermes ai-rulez source → $VERSION"
VERSION="$VERSION" perl -0pi -e \
  's/(\[plugin\][^\[]*?\nversion\s*=\s*")[^"]+(")/$1$ENV{VERSION}$2/s' plugin/.ai-rulez/config.plugins.toml

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

python3 scripts/sync_plugin_version.py
AI_RULEZ="$AI_RULEZ" bash scripts/sync-plugin-skills.sh

if [[ "$CURRENT_RELEASE_MINOR" != "$RELEASE_MINOR" ]]; then
  echo "→ RELEASE_MINOR      $CURRENT_RELEASE_MINOR → $RELEASE_MINOR (minor bump — schema wipe on next scan)"
  sed -i.bak -E "s/^pub const RELEASE_MINOR: u16 = [0-9]+;/pub const RELEASE_MINOR: u16 = $RELEASE_MINOR;/" src/version.rs
  rm src/version.rs.bak
else
  echo "→ RELEASE_MINOR      $CURRENT_RELEASE_MINOR (no change — patch bump)"
fi

# Rewrites the version of the basemind entry in Cargo.lock without compiling anything; a failure ~keep
# here (an unresolvable manifest, no network for a missing index entry) stops the bump. ~keep
cargo metadata --format-version 1 >/dev/null

echo
echo "Validating version consistency across all surfaces..."
python3 scripts/check_release_versions.py "$VERSION"

echo
echo "Done. Review with: git diff"
echo "Next: cargo test --workspace && git commit -am 'chore(release): v$VERSION'"
