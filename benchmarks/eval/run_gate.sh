#!/usr/bin/env bash
# Run the ADR-0022 value gate: fetch and verify the corpus, scan each repository into an isolated
# cache, run its committed task file through `basemind admin eval`, and write the per-tool verdict.
#
#   BASEMIND_DATA_HOME=$(mktemp -d) BASEMIND_COMMS_DIR=$(mktemp -d) \
#     benchmarks/eval/run_gate.sh BASEMIND_BIN CORPUS_DIR [RESULTS_DIR]
#
# BASEMIND_BIN must be built with the `tokenizer` feature (o200k counts; ADR-0022 rejects bytes/4).
# Both cache directories must be set, exist and be empty: a run against the default cache could
# take over a live daemon and index. RESULTS_DIR defaults to `benchmarks/eval/results`.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
bin="${1:?usage: run_gate.sh BASEMIND_BIN CORPUS_DIR [RESULTS_DIR]}"
corpus="${2:?usage: run_gate.sh BASEMIND_BIN CORPUS_DIR [RESULTS_DIR]}"
results="${3:-${here}/results}"

for var in BASEMIND_DATA_HOME BASEMIND_COMMS_DIR; do
  dir="${!var:-}"
  if [ -z "${dir}" ] || [ ! -d "${dir}" ]; then
    echo "refusing to run: ${var} must name an existing fresh directory (mktemp -d)" >&2
    exit 2
  fi
  if [ -n "$(ls -A "${dir}")" ]; then
    echo "refusing to run: ${var}=${dir} is not empty; use a fresh mktemp -d" >&2
    exit 2
  fi
  case "$(cd "${dir}" && pwd -P)/" in
  "${HOME}/.local/share/basemind/"* | "${HOME}/Library/"*)
    echo "refusing to run: ${var}=${dir} is a default basemind location" >&2
    exit 2
    ;;
  esac
done
export BASEMIND_DATA_HOME BASEMIND_COMMS_DIR
export RUST_LOG="${RUST_LOG:-error}"

stop_daemon() {
  # A build with `comms` may have started a daemon on this cache; stop it, never another one.
  if "${bin}" comms --help >/dev/null 2>&1; then
    "${bin}" comms stop >/dev/null 2>&1 || true
  fi
}
trap stop_daemon EXIT

# `admin tokens` fails outright without the `tokenizer` feature rather than falling back to bytes/4.
if ! printf 'hello world' | "${bin}" admin tokens --stdin >/dev/null 2>&1; then
  echo "refusing to run: ${bin} does not count o200k tokens (build it with --features tokenizer)" >&2
  exit 2
fi

"${here}/fetch_corpus.sh" "${corpus}"
corpus="$(cd "${corpus}" && pwd -P)"
mkdir -p "${results}"
version="$("${bin}" --version)"
run_log="${results}/run.tsv"
printf 'corpus\tcommit\ttasks\tscan_s\teval_s\tload_before\tload_after\tbinary\n' >"${run_log}"

while IFS=$'\t' read -r name _ commit _; do
  case "${name}" in '' | '#'*) continue ;; esac
  repo="${corpus}/${name}"
  tasks="${here}/tasks/${name}.jsonl"
  load_before="$(uptime | sed 's/.*load averages*: *//')"
  t0="$(date +%s)"
  "${bin}" --root "${repo}" -q scan >/dev/null
  t1="$(date +%s)"
  "${bin}" --root "${repo}" admin eval --tasks "${tasks}" --min-recall 0.8 \
    --out "${results}/${name}.ndjson" --report "${results}/${name}.report.json" \
    --markdown "${results}/${name}.md" 2>"${results}/${name}.stderr"
  t2="$(date +%s)"
  # The reports name the machine's checkout and task paths; commit them machine-independent.
  python3 - "${corpus}" "${here}" "${results}/${name}.report.json" "${results}/${name}.md" <<'PY'
import sys
from pathlib import Path

corpus, here, *files = sys.argv[1:]
for f in map(Path, files):
    f.write_text(f.read_text().replace(corpus.rstrip("/") + "/", "$CORPUS/").replace(here + "/", "benchmarks/eval/"))
PY
  load_after="$(uptime | sed 's/.*load averages*: *//')"
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "${name}" "${commit}" "$(grep -vc '^#' "${tasks}")" \
    "$((t1 - t0))" "$((t2 - t1))" "${load_before}" "${load_after}" "${version}" >>"${run_log}"
  echo "${name}: scan $((t1 - t0))s, eval $((t2 - t1))s (load ${load_before} -> ${load_after})"
done <"${here}/corpus.tsv"

python3 "${here}/gate.py" "${results}" >"${results}/gate.md"
cat "${results}/gate.md"
