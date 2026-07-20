#!/usr/bin/env bash
# Dry-run the REAL release packaging path for the host platform: build the binary, run the same ~keep
# scripts/package-release.sh the publish workflow uses, generate the checksums file the launcher / ~keep
# npm / pip verify against, then extract and smoke-test the archive (binary runs, version matches, ~keep
# and the plugin launcher accepts it through its BASEMIND_BIN version gate). basemind has no ~keep
# GoReleaser config, so the old `goreleaser release --snapshot` task validated nothing real. ~keep
# Env: RELEASE_DRY_RUN_NO_BUILD=1 reuses an existing binary; RELEASE_DRY_RUN_FEATURES overrides. ~keep
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

TRIPLE="$(rustc -vV | sed -n 's/^host: //p')"
FEATURES="${RELEASE_DRY_RUN_FEATURES-full}"
VERSION="$(sed -n 's/^version = "\([^"]*\)".*/\1/p' Cargo.toml | head -n1)"

case "$TRIPLE" in
*windows*) EXT="zip" ;;
*) EXT="tar.gz" ;;
esac
ARCHIVE="basemind-${TRIPLE}.${EXT}"
SUMS="basemind_${VERSION}_checksums.txt"
STAGING="basemind-staging-${TRIPLE}"

WORK=""
cleanup() {
	rm -rf "${WORK:-}" "$ARCHIVE" "$SUMS" "$STAGING" 2>/dev/null || true
}
trap cleanup EXIT

if [ "${RELEASE_DRY_RUN_NO_BUILD:-}" != "1" ]; then
	echo "==> building basemind ${VERSION} (${TRIPLE}, features: '${FEATURES:-<default>}')"
	if [ -n "$FEATURES" ]; then
		cargo build --release --features "$FEATURES" --bin basemind --target "$TRIPLE"
	else
		cargo build --release --bin basemind --target "$TRIPLE"
	fi
fi

echo "==> packaging via scripts/package-release.sh ${TRIPLE}"
./scripts/package-release.sh "$TRIPLE"
[ -f "$ARCHIVE" ] || {
	echo "error: expected archive ${ARCHIVE} was not produced" >&2
	exit 1
}

echo "==> generating ${SUMS}"
if command -v sha256sum >/dev/null 2>&1; then
	sha256sum "$ARCHIVE" >"$SUMS"
else
	shasum -a 256 "$ARCHIVE" >"$SUMS"
fi

echo "==> smoke-testing the packaged artifact"
WORK="$(mktemp -d)"
case "$EXT" in
tar.gz) tar -xzf "$ARCHIVE" -C "$WORK" ;;
zip) unzip -qo "$ARCHIVE" -d "$WORK" ;;
esac
BIN="$WORK/basemind"
[ "$EXT" = "zip" ] && BIN="$WORK/basemind.exe"
[ -x "$BIN" ] || {
	echo "error: basemind binary missing from ${ARCHIVE}" >&2
	exit 1
}
GOT="$("$BIN" --version | awk '{print $2}')"
[ "$GOT" = "$VERSION" ] || {
	echo "error: packaged version ${GOT} != Cargo.toml ${VERSION}" >&2
	exit 1
}

BASEMIND_BIN="$BIN" CLAUDE_PLUGIN_ROOT="$PWD" scripts/mcp-launch.sh --version >/dev/null

echo "OK: ${ARCHIVE} + ${SUMS} (basemind ${VERSION}, ${TRIPLE}) packaged and smoke-tested"
