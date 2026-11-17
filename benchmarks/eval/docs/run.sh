#!/usr/bin/env bash
# Scan the docs corpus as its own workspace and score `docs_search` on it with `basemind admin eval`.
#
#   BASEMIND_DATA_HOME=$(mktemp -d) BASEMIND_COMMS_DIR=$(mktemp -d) \
#     benchmarks/eval/docs/run.sh BASEMIND_BIN OUT_DIR [TASKS]
#
# BASEMIND_BIN needs the `documents` feature. Both cache directories must exist and be empty. The
# corpus is copied to OUT_DIR/workspace with a `basemind.toml` that turns documents on, so the scan
# indexes it under its own root. Writes OUT_DIR/report.json, report.md, results.ndjson and scan.log.
#
#   EVAL_RERANK=1        turn `[documents.reranker]` on.
#   EVAL_DISTRACTORS=N   add N Markdown distractor documents (grow_corpus.py), so the corpus holds more
#                        chunks than a lane's candidate cut and a filter has to act before it.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
bin="${1:?usage: run.sh BASEMIND_BIN OUT_DIR [TASKS]}"
out="${2:?usage: run.sh BASEMIND_BIN OUT_DIR [TASKS]}"
tasks="${3:-${here}/tasks.jsonl}"

for var in BASEMIND_DATA_HOME BASEMIND_COMMS_DIR; do
  dir="${!var:-}"
  if [ -z "${dir}" ] || [ ! -d "${dir}" ] || [ -n "$(ls -A "${dir}")" ]; then
    echo "refusing to run: ${var} must name an existing empty directory (mktemp -d)" >&2
    exit 2
  fi
done

mkdir -p "${out}"
workspace="${out}/workspace"
rm -rf "${workspace}"
cp -R "${here}/corpus" "${workspace}"
cat >"${workspace}/basemind.toml" <<'EOF'
"$schema" = "v1"

[documents]
enabled = true
embed = true
EOF
if [ "${EVAL_RERANK:-0}" = "1" ]; then
  printf '\n[documents.reranker]\nenabled = true\n' >>"${workspace}/basemind.toml"
fi
if [ "${EVAL_DISTRACTORS:-0}" -gt 0 ]; then
  python3 -I "${here}/grow_corpus.py" "${tasks}" "${workspace}" "${EVAL_DISTRACTORS}"
fi

"${bin}" --root "${workspace}" scan >"${out}/scan.log" 2>&1
"${bin}" --root "${workspace}" admin eval --tasks "${tasks}" --mode docs --warmup \
  --out "${out}/results.ndjson" --report "${out}/report.json" --markdown "${out}/report.md"
