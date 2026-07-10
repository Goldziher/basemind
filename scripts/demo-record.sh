#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

MEDIA_DIR="docs/media"
CAST="$MEDIA_DIR/demo.cast"
GIF="$MEDIA_DIR/demo.gif"

need() {
	if ! command -v "$1" >/dev/null 2>&1; then
		printf 'demo-record: missing %s — install it first (macOS: brew install asciinema agg)\n' "$1" >&2
		exit 1
	fi
}
need asciinema
need agg

if [ ! -x "target/release/basemind" ] && [ -z "${BASEMIND_BIN:-}" ]; then
	printf 'demo-record: building release binary ...\n' >&2
	cargo build --release
fi

mkdir -p "$MEDIA_DIR"

printf 'demo-record: recording cast → %s\n' "$CAST" >&2
asciinema rec --overwrite --cols 90 --rows 28 \
	--command "./scripts/demo.sh" "$CAST"

printf 'demo-record: rendering GIF → %s\n' "$GIF" >&2
agg --theme monokai --font-size 16 "$CAST" "$GIF"

printf 'demo-record: done. Review %s, then commit %s + %s.\n' "$GIF" "$CAST" "$GIF" >&2
