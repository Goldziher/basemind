#!/usr/bin/env bash
# Run the ADR-0022 value gate: fetch and verify the corpus, scan each repository into an isolated
# cache, run its committed task file through `basemind admin eval`, and write the per-tool verdict.
#
#   BASEMIND_DATA_HOME=$(mktemp -d) BASEMIND_COMMS_DIR=$(mktemp -d) \
#     benchmarks/eval/run_gate.sh BASEMIND_BIN CORPUS_DIR [RESULTS_DIR]
#
# BASEMIND_BIN must be built with the `tokenizer` feature (o200k counts; ADR-0022 rejects bytes/4) and
# `code-intel` (the Rust import resolver the `impact` and `cycles` kinds need). The `read_around`
# baseline is what `rg` prints, so `rg` must be on PATH (`mise install` provides the pinned one);
# its version goes into `run.tsv`.
# Both cache directories must be set, exist and be empty: a run against the default cache could
# take over a live daemon and index. RESULTS_DIR defaults to `benchmarks/eval/results`.
# Run it from the checkout BASEMIND_BIN was built from: `run.tsv` records that checkout's commit
# (`-dirty` with uncommitted tracked changes) as the binary's source, beside the build hash and the
# features the binary reports, because the version string does not tell builds apart.
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
# The clones, the scans and every baseline command run without the host's system or global git
# configuration and in the C locale: a `core.autocrlf` or `color.ui` there changes what is checked
# out and what `git` prints, and so the baseline token counts.
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null LC_ALL=C

stop_daemon() {
  # A build with `comms` may have started a daemon on this cache; stop it, never another one.
  if "${bin}" comms --help >/dev/null 2>&1; then
    "${bin}" comms stop >/dev/null 2>&1 || true
  fi
}
trap stop_daemon EXIT

# `admin tokens` fails outright without the `tokenizer` feature rather than falling back to bytes/4,
# but with it the count silently becomes a word estimate when the tokenizer cannot load (offline).
# The probe has a known o200k count (crates/basemind-engine/src/engine/tokens.rs `O200K_PROBE`) that the estimate misses.
if ! probe_count="$(printf '%s' 'Hello, world!' | "${bin}" admin tokens --stdin 2>/dev/null)"; then
  echo "refusing to run: ${bin} does not count o200k tokens (build it with --features tokenizer)" >&2
  exit 2
fi
if [ "${probe_count}" != "4" ]; then
  echo "refusing to run: ${bin} counted ${probe_count} tokens for the o200k probe, expected 4; the tokenizer fell back to the word estimate (offline? download Xenova/gpt-4o first)" >&2
  exit 2
fi

# The `read_around` baseline runs `rg`; without it every such baseline fails instead of measuring.
rg_version="none"
while IFS=$'\t' read -r name _; do
  case "${name}" in '' | '#'*) continue ;; esac
  if grep -q -E '"mode": *"read_around"' "${here}/tasks/${name}.jsonl"; then
    if ! command -v rg >/dev/null 2>&1; then
      echo "refusing to run: tasks/${name}.jsonl has read_around tasks, whose baseline is rg output; install rg" >&2
      exit 2
    fi
    rg_version="$(rg --version | head -n 1)"
  fi
done <"${here}/corpus.tsv"

"${here}/fetch_corpus.sh" "${corpus}"
corpus="$(cd "${corpus}" && pwd -P)"
mkdir -p "${results}"
version="$("${bin}" --version)"
source_commit="$(git -C "${here}" rev-parse HEAD)"
if [ -n "$(git -C "${here}" status --porcelain --untracked-files=no)" ]; then
  source_commit="${source_commit}-dirty"
fi
run_log="${results}/run.tsv"
printf 'corpus\tcommit\ttasks\tscan_s\teval_s\tload_before\tload_after\tbinary\tsource_commit\tbuild_id\tfeatures\trg\n' \
  >"${run_log}"

while IFS=$'\t' read -r name _ commit _ _ languages; do
  case "${name}" in '' | '#'*) continue ;; esac
  repo="${corpus}/${name}"
  tasks="${here}/tasks/${name}.jsonl"
  load_before="$(uptime | sed 's/.*load averages*: *//')"
  t0="$(date +%s)"
  "${bin}" --root "${repo}" -q scan >/dev/null
  t1="$(date +%s)"
  # Without an import resolver for a language (`code-intel`) its graph is empty and every `impact` /
  # `cycles` task scores 0 as if the tool were wrong; refuse instead of reporting that as a verdict.
  # The probe reads the structured `coverage` of `code cycles`: each language the corpus has graph
  # tasks for (rust, python, typescript) must have a resolver row and not be listed under
  # `without_resolver`.
  if grep -q -E '"mode": *"(impact|cycles)"' "${tasks}"; then
    for language in rust python typescript; do
      case ",${languages}," in *",${language},"*) ;; *) continue ;; esac
      if ! "${bin}" --root "${repo}" code cycles --json 2>/dev/null | python3 -c '
import json, sys
cov = json.load(sys.stdin)["coverage"]
language = sys.argv[1]
has_row = any(row["language"] == language for row in cov["languages"])
sys.exit(0 if has_row and language not in cov.get("without_resolver", []) else 1)
' "${language}"; then
        echo "refusing to run: ${bin} resolves no ${name} ${language} imports (build it with --features tokenizer,code-intel)" >&2
        exit 2
      fi
    done
  fi
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
  build="$(python3 -c '
import json, sys
b = json.load(open(sys.argv[1])).get("build", {})
print(b.get("build_id", "unknown") + "\t" + (",".join(b.get("features", [])) or "none"))
' "${results}/${name}.report.json")"
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "${name}" "${commit}" "$(grep -vc '^#' "${tasks}")" \
    "$((t1 - t0))" "$((t2 - t1))" "${load_before}" "${load_after}" "${version}" "${source_commit}" "${build}" \
    "${rg_version}" >>"${run_log}"
  echo "${name}: scan $((t1 - t0))s, eval $((t2 - t1))s (load ${load_before} -> ${load_after})"
done <"${here}/corpus.tsv"

python3 "${here}/gate.py" "${results}" >"${results}/gate.md"
cat "${results}/gate.md"
