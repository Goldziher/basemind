#!/usr/bin/env bash
# Bench gitmind against a handful of real-world OSS repos.
# Clones into /tmp/gitmind-bench/ (skips if already present) and runs cold + cached scans.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT_DIR/target/release/gitmind"
BENCH_DIR="${GITMIND_BENCH_DIR:-/tmp/gitmind-bench}"

if [[ ! -x "$BIN" ]]; then
  echo "building release binary..."
  (cd "$ROOT_DIR" && cargo build --release --bin gitmind)
fi

mkdir -p "$BENCH_DIR"
cd "$BENCH_DIR"

declare -a REPOS=(
  "https://github.com/BurntSushi/ripgrep|rust"
  "https://github.com/psf/requests|python"
  "https://github.com/gin-gonic/gin|go"
)

for entry in "${REPOS[@]}"; do
  url="${entry%%|*}"
  tag="${entry##*|}"
  name="$(basename "$url")"
  if [[ ! -d "$name" ]]; then
    echo "==> cloning $name ($tag)"
    git clone --depth 1 -q "$url" "$name"
  fi
  cd "$name"
  rm -rf .gitmind
  "$BIN" init >/dev/null

  echo
  echo "==> $name — cold scan"
  /usr/bin/time -p "$BIN" scan 2>&1 | tail -5

  echo "==> $name — cached scan"
  /usr/bin/time -p "$BIN" scan 2>&1 | tail -5

  blob_count="$(find .gitmind/blobs -type f -name '*.l1.msgpack' | wc -l | tr -d ' ')"
  idx_bytes="$(wc -c <.gitmind/index.msgpack | tr -d ' ')"
  echo "    blobs=$blob_count  index_bytes=$idx_bytes"

  cd "$BENCH_DIR"
done

echo
echo "done."
