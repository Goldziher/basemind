#!/usr/bin/env bash
# benchmarks/retrieval/run.sh <work-dir> <out.ndjson> <corpus-size>...
#
# Drives the retrieval backend matrix: for each corpus size, generate the corpus and its brute-force
# truth, then for each arm ingest, index, query at 100% / 10% / 1% filter selectivity crossed with
# 100% / 90% / 50% live documents, query under a 2,000- and a 20,000-key liveness list, append 1% of
# rows, query, optimize, query. Every step is its own process,
# wrapped in /usr/bin/time so peak RSS is that step's alone; the load average is recorded beside it. Measurements are appended to
# <out.ndjson>; judge them with decide.py.
#
# Everything is written under <work-dir>, which must already exist and be empty. Corpora and stores
# are deleted as soon as they have been measured unless KEEP=1.
#
# Environment:
#   BENCH_BIN     path to the release `retrieval_bench` example (required)
#   ARMS          arms to run (default "A B"; arm C is measured only when arm B fails a row)
#   LIVES         live-document shares to cross with the selectivities (default "100 90 50")
#   DIM, SEED     corpus dimension and seed (default 768, 42)
#   B_KNOBS       extra arm B flags, for example "--ann --nprobes 20 --refine-factor 2"
#   KEEP=1        keep corpora and stores for the probes
set -euo pipefail

die() {
  echo "error: $*" >&2
  exit 1
}

[[ $# -ge 3 ]] || die "usage: $0 <work-dir> <out.ndjson> <corpus-size>..."
WORK="$1"
OUT="$2"
shift 2
[[ -n "${BENCH_BIN:-}" && -x "$BENCH_BIN" ]] || die "set BENCH_BIN to the release retrieval_bench example"
[[ -d "$WORK" ]] || die "work dir does not exist: $WORK"
[[ -z "$(ls -A "$WORK")" ]] || die "work dir is not empty: $WORK"

ARMS="${ARMS:-A B}"
LIVES="${LIVES:-100 90 50}"
DIM="${DIM:-768}"
SEED="${SEED:-42}"
read -r -a B_KNOBS_ARRAY <<<"${B_KNOBS:-}"

case "$(uname -s)" in
Darwin) TIME_FLAG="-l" ;;
*) TIME_FLAG="-v" ;;
esac

# Peak RSS in bytes from the `time` report: macOS prints bytes, GNU time prints kibibytes.
peak_rss() {
  awk '
    /maximum resident set size/ { print $1; found = 1 }
    /Maximum resident set size/ { print $NF * 1024; found = 1 }
    END { if (!found) print 0 }
  ' "$1"
}

# One-minute load average when a step ends: a latency measured beside other work is not comparable.
load1() {
  if [[ -r /proc/loadavg ]]; then
    cut -d' ' -f1 /proc/loadavg
  else
    sysctl -n vm.loadavg | awk '{ print $2 }'
  fi
}

# measure <arm> <size> <step> <command...>: run one process, keep its NDJSON, record its peak RSS.
measure() {
  local arm="$1" size="$2" step="$3"
  shift 3
  local report="$WORK/time.txt" lines="$WORK/step.ndjson"
  echo "[$(date +%H:%M:%S)] $size $arm $step" >&2
  if ! /usr/bin/time "$TIME_FLAG" "$@" >"$lines" 2>"$report"; then
    cat "$report" >&2
    die "step failed: $size $arm $step"
  fi
  [[ -s "$lines" ]] || die "step printed no measurement: $size $arm $step"
  cat "$lines" >>"$OUT"
  printf '{"kind":"rss","arm":"%s","n":%s,"step":"%s","peak_rss_bytes":%s,"load1":%s}\n' \
    "$arm" "$size" "$step" "$(peak_rss "$report")" "$(load1)" >>"$OUT"
}

for size in "$@"; do
  corpus="$WORK/corpus-$size"
  measure "-" "$size" "gen" "$BENCH_BIN" gen "$size" "$DIM" "$SEED" "$corpus"
  measure "-" "$size" "truth-base" "$BENCH_BIN" truth "$corpus" base
  measure "-" "$size" "truth-appended" "$BENCH_BIN" truth "$corpus" appended

  for arm in $ARMS; do
    store="$WORK/store-$size-$arm"
    knobs=()
    if [[ "$arm" == "B" ]]; then
      knobs=(${B_KNOBS_ARRAY[@]+"${B_KNOBS_ARRAY[@]}"})
    fi
    build() {
      measure "$arm" "$size" "build-$1" "$BENCH_BIN" build "$arm" "$1" "$corpus" "$store" ${knobs[@]+"${knobs[@]}"}
    }
    query() {
      local step="$1"
      shift
      measure "$arm" "$size" "$step" "$BENCH_BIN" query "$arm" "$corpus" "$store" "$@" ${knobs[@]+"${knobs[@]}"}
    }
    build ingest
    build index
    for live in $LIVES; do
      for selectivity in 100 10 1; do
        query "query-base-sel$selectivity-live$live" --selectivity "$selectivity" --live "$live" --state base
      done
    done
    for keys in 2000 20000; do
      query "query-base-keys$keys" --key-list "$keys" --state base
    done
    build append
    query "query-appended" --state appended
    build optimize
    query "query-optimized" --state optimized
    [[ "${KEEP:-0}" == "1" ]] || rm -rf "$store"
  done
  [[ "${KEEP:-0}" == "1" ]] || rm -rf "$corpus"
done
rm -f "$WORK/time.txt" "$WORK/step.ndjson"
echo "wrote $OUT" >&2
