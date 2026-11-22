#!/usr/bin/env bash
# Record the README demos with vhs: `demo-record.sh [cli|mcp|comms|all]` (default all).
# BASEMIND_BIN names the binary to record (else target/release/basemind, else PATH).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

for tool in vhs ffmpeg git python3; do
  command -v "$tool" >/dev/null 2>&1 || {
    printf 'demo-record: missing %s\n' "$tool" >&2
    exit 1
  }
done

if [ -n "${BASEMIND_BIN:-}" ]; then
  BIN="$BASEMIND_BIN"
elif [ -x target/release/basemind ]; then
  BIN="$REPO_ROOT/target/release/basemind"
else
  BIN="$(command -v basemind)"
fi
[ -x "$BIN" ] || {
  printf 'demo-record: no basemind binary; set BASEMIND_BIN\n' >&2
  exit 1
}

# Unix socket paths are limited to ~100 bytes, so the comms dir lives directly under /tmp.
TMP="$(mktemp -d)"
COMMS="$(mktemp -d /tmp/bmdemo.XXXX)"
trap 'rm -rf "$TMP" "$COMMS"' EXIT
mkdir -p "$TMP/bin"
ln -s "$BIN" "$TMP/bin/basemind"

# Env for the recorded shell, sourced while the recording is hidden.
cat >"$TMP/env.sh" <<ENV
export PS1='\$ ' PATH="$TMP/bin:\$PATH" RUST_LOG=error BASEMIND_BIN="$TMP/bin/basemind"
export BASEMIND_DATA_HOME="$TMP/data" BASEMIND_COMMS_DIR="$COMMS" BASEMIND_AGENT_ID=demo-recorder
ENV

tape() { # <output> <width> <height> <font> ; body on stdin
  {
    printf 'Output "%s"\nSet Shell "bash"\nSet FontSize %s\nSet Width %s\nSet Height %s\n' "$1" "$4" "$2" "$3"
    printf 'Set Padding 16\nSet Theme "Monokai Remastered"\nSet TypingSpeed 0\n'
    printf 'Hide\nType "source %s/env.sh"\nEnter\n' "$TMP"
    cat
  } >"$TMP/rec.tape"
  vhs "$TMP/rec.tape"
}

# The pre-commit hook rejects files over 500 KiB, so shrink the long CLI recording.
shrink() {
  ffmpeg -y -loglevel error -i "$1" -vf "fps=8,scale=960:-1:flags=lanczos,palettegen=max_colors=32:stats_mode=diff" "$TMP/pal.png"
  ffmpeg -y -loglevel error -i "$1" -i "$TMP/pal.png" \
    -lavfi "fps=8,scale=960:-1:flags=lanczos[x];[x][1:v]paletteuse=dither=none" "$TMP/small.gif"
  mv "$TMP/small.gif" "$1"
}

cli() {
  tape "$REPO_ROOT/docs/media/demo.gif" 1100 700 15 <<TAPE
Type "clear; cd $REPO_ROOT && ./scripts/demo.sh"
Enter
Wait@900s /❯/
Show
Wait+Screen@300s /scanner_drive_failure/
Sleep 2s
TAPE
  shrink "$REPO_ROOT/docs/media/demo.gif"
}

# A public project to point the MCP and comms demos at.
sample_repo() {
  [ -d "$TMP/requests" ] ||
    git clone -q --depth 1 https://github.com/psf/requests "$TMP/requests"
  cp scripts/demo-mcp.py scripts/demo-comms.sh "$TMP/requests/"
  (cd "$TMP/requests" && PATH="$TMP/bin:$PATH" RUST_LOG=error \
    BASEMIND_DATA_HOME="$TMP/data" BASEMIND_COMMS_DIR="$COMMS" basemind scan --quiet >/dev/null)
}

mcp() {
  sample_repo
  tape "$REPO_ROOT/docs/media/mcp-demo.gif" 1100 600 15 <<TAPE
Type "cd $TMP/requests && clear"
Enter
Sleep 2s
Show
Type "python3 demo-mcp.py basemind . src/requests/sessions.py get_adapter"
Enter
Wait+Screen@60s /total: 11/
Sleep 3s
TAPE
}

comms() {
  sample_repo
  tape "$REPO_ROOT/docs/demos/comms-demo.gif" 1200 520 14 <<TAPE
Type "cd $TMP/requests && clear"
Enter
Sleep 2s
Show
Type "./demo-comms.sh"
Enter
Wait+Screen@90s /No findings\./
Wait+Screen@30s /reviewer\s+review/
Sleep 3s
TAPE
}

case "${1:-all}" in
cli) cli ;;
mcp) mcp ;;
comms) comms ;;
all)
  cli
  mcp
  comms
  ;;
*)
  printf 'usage: demo-record.sh [cli|mcp|comms|all]\n' >&2
  exit 2
  ;;
esac
