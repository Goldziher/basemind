#!/usr/bin/env bash
# Atomically bump the basemind version across every shipped surface.
#
# Usage: ./scripts/release-bump.sh <version>
# Example: ./scripts/release-bump.sh 0.1.0
# Example: ./scripts/release-bump.sh 0.2.0-rc.1
#
# Surfaces touched:
#   Cargo.toml                            [package] version
#   npm-package/package.json              "version"
#   pip-package/pyproject.toml            version (PyPI form: 0.1.0-rc.1 → 0.1.0rc1)
#   pip-package/basemind/__init__.py       __version__
#   src/version.rs                        RELEASE_MINOR (if minor changed)
#   package.json                          "version" (root, workspace marker)
#   opencode-plugin/package.json          "version" (basemind-opencode npm pkg)
#   .claude-plugin/plugin.json            "version"
#   .claude-plugin/marketplace.json       plugins[0].version
#   .codex-plugin/plugin.json             "version"
#   .cursor-plugin/plugin.json            "version"
#   gemini-extension.json                 "version"
#
# If the minor component changed, RELEASE_MINOR is also bumped to track. Patch-only
# bumps leave RELEASE_MINOR alone so existing user caches don't wipe on patch upgrade.

set -euo pipefail

VERSION="${1:?usage: release-bump.sh <version>}"

# Reject anything that isn't <major>.<minor>.<patch>(-rc.<n>)?
if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?$ ]]; then
  echo "error: version must be MAJOR.MINOR.PATCH or MAJOR.MINOR.PATCH-rc.N (got '$VERSION')" >&2
  exit 1
fi

# PyPI form: 0.1.0-rc.1 → 0.1.0rc1
PY_VERSION="${VERSION//-rc./rc}"

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

# Derive minor (decimal): 0.X.y → X; M.X.y → M*100 + X.
MAJOR="$(echo "$VERSION" | cut -d. -f1)"
MINOR="$(echo "$VERSION" | cut -d. -f2)"
RELEASE_MINOR=$((MAJOR * 100 + MINOR))

# Special-case 0.x for readability: keep RELEASE_MINOR == MINOR for the entire 0.x line.
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

# Plugin manifests (per-harness) — every shipped manifest carries the same Cargo version (no PyPI
# normalisation). Bump the version VALUE in place with a surgical sed so existing formatting is
# preserved: a jq re-serialization pretty-prints and EXPANDS short arrays, which then fights the
# `oxfmt` prek hook (it collapses them) and produces churn on every release. Each manifest carries
# exactly one `"version"` key — including marketplace.json, whose only version is `plugins[0].version`
# — so a single global substitution is unambiguous.
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
bump_json_version .claude-plugin/marketplace.json

if [[ "$CURRENT_RELEASE_MINOR" != "$RELEASE_MINOR" ]]; then
  echo "→ RELEASE_MINOR      $CURRENT_RELEASE_MINOR → $RELEASE_MINOR (minor bump — schema wipe on next scan)"
  sed -i.bak -E "s/^pub const RELEASE_MINOR: u16 = [0-9]+;/pub const RELEASE_MINOR: u16 = $RELEASE_MINOR;/" src/version.rs
  rm src/version.rs.bak
else
  echo "→ RELEASE_MINOR      $CURRENT_RELEASE_MINOR (no change — patch bump)"
fi

# Cargo.lock follows from a build.
cargo build --quiet 2>/dev/null || true

echo
echo "Done. Review with: git diff"
echo "Next: cargo test --workspace && git commit -am 'chore(release): v$VERSION'"
