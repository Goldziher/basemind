#!/usr/bin/env bash
# Drives examples/doc_measure.rs: extract_doc time and peak RSS vs input size, xberg splitter vs linear chunker.
# Usage: scripts/doc_measure.sh <profile: none|embed|full> "<kinds>" "<sizes-bytes>" [timeout-secs]
# Needs a release build: cargo build --release --features full --example doc_measure
# Output: one line per sample: profile kind bytes mode rss_mib=<peak> <doc_measure key=value fields>
set -u
here="$(cd "$(dirname "$0")/.." && pwd)"
bin="${CARGO_TARGET_DIR:-$here/target}/release/examples/doc_measure"
profile="$1"
kinds="$2"
sizes="$3"
limit="${4:-300}"
work="${DOC_MEASURE_DIR:-/tmp/bm-doc-measure}"
mkdir -p "$work"
mime_for() {
  case "$1" in
  log) echo text/plain ;; csv) echo text/csv ;; yaml) echo application/yaml ;;
  json) echo application/json ;; xml) echo application/xml ;; *md) echo text/markdown ;;
  esac
}
for kind in $kinds; do
  for size in $sizes; do
    file="$work/$kind-$size.txt"
    [ -f "$file" ] || "$bin" gen "$kind" "$size" "$file"
    for mode in xberg linear; do
      out="$work/.out.$$"
      /usr/bin/time -l timeout "$limit" "$bin" run "$file" "$(mime_for "$kind")" "$mode" "$profile" >"$out" 2>"$out.err"
      rc=$?
      rss=$(awk '/maximum resident set size/ {printf "%.1f", $1/1048576}' "$out.err")
      line=$(cat "$out")
      [ "$rc" -eq 124 ] && line="status=timeout>${limit}s"
      echo "$profile $kind $size $mode rss_mib=$rss ${line:-status=rc$rc}"
    done
  done
done
rm -f "$work/.out.$$" "$work/.out.$$.err"
