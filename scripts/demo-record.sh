#!/usr/bin/env bash
# Record the basemind CLI demo to an asciinema cast and render it to a GIF.
#
# Drives scripts/demo.sh under `asciinema rec`, then converts the cast to
# docs/media/demo.gif with `agg`. Commit both: the .cast is the re-recordable
# source of truth, the .gif is what the README embeds.
#
# Usage: ./scripts/demo-record.sh   (or: task demo:record)
# Needs: asciinema + agg on PATH  (macOS: brew install asciinema agg)
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

# A release binary makes the recording fast and representative. Build if absent.
if [ ! -x "target/release/basemind" ] && [ -z "${BASEMIND_BIN:-}" ]; then
	printf 'demo-record: building release binary ...\n' >&2
	cargo build --release
fi

mkdir -p "$MEDIA_DIR"

printf 'demo-record: recording cast → %s\n' "$CAST" >&2
# 90x28 keeps the GIF legible and README-sized; --command runs the demo
# non-interactively and exits, so the cast has no trailing idle.
asciinema rec --overwrite --cols 90 --rows 28 \
	--command "./scripts/demo.sh" "$CAST"

printf 'demo-record: rendering GIF → %s\n' "$GIF" >&2
# --theme: light-on-dark; --font-size tuned for README width. agg picks a
# default monospace font; pass --font-family if the default is unavailable.
agg --theme monokai --font-size 16 "$CAST" "$GIF"

printf 'demo-record: done. Review %s, then commit %s + %s.\n' "$GIF" "$CAST" "$GIF" >&2
