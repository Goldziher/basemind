#!/usr/bin/env bash
# Regenerate the committed value-gate task files from a fetched corpus (ADR-0022: `--seed 1`, one
# file per corpus, regenerated only when a pin in `corpus.tsv` moves).
#
#   benchmarks/eval/gen_tasks.sh CORPUS_DIR [OUT_DIR]
#
# OUT_DIR defaults to `benchmarks/eval/tasks`. Needs only git, python3 and cargo (to build
# `rust_gold` offline from the cargo cache); no basemind binary is involved, so no gold comes from
# the tool under test. A corpus lists its languages in `corpus.tsv`; each language gets the kinds
# whose gold this harness can derive for it (README, "Generators"):
#
#   rust        symbols, outline, references, callers, implementations, dependents, read (`rust_gold`),
#               every git kind, impact/cycles over `use`, explain
#   python      symbols, outline, references, callers, dependents (`ast`), impact/cycles over `import`
#   typescript  symbols, references, dependents (`_langs.py`, from the text), impact/cycles over relative imports and
#               barrels
#   go          symbols, references, dependents (`_langs.py`)
#   java        symbols, references (`_langs.py`; basemind indexes no Java imports, so no dependents)
#   every one   grep, grep_changed, read_around, find_large, git_path, blame, diff (`git` and text only)
#
# `find` and `git_search` span the whole repository and are generated once. A corpus with several
# languages suffixes each language's task ids with `-<ext>`, so `gate.py` can report recall per language.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
corpus="${1:?usage: gen_tasks.sh CORPUS_DIR [OUT_DIR]}"
out="${2:-${here}/tasks}"
seed=1
n=25
mkdir -p "${out}"

ext_of() {
  case "$1" in
  rust) echo rs ;;
  python) echo py ;;
  typescript) echo ts ;;
  go) echo go ;;
  java) echo java ;;
  *)
    echo "gen_tasks.sh: unknown language '$1' in corpus.tsv" >&2
    return 1
    ;;
  esac
}

# gen SCRIPT REPO OUT [ARGS...]: one generator at the corpus seed and count.
gen() {
  local script="$1" repo="$2" dest="$3"
  shift 3
  # `exit`, not `return`: the callers run in command substitutions, whose subshells do not inherit -e.
  python3 "${here}/${script}" --repo "${repo}" --seed "${seed}" --n "${n}" --out "${dest}" "$@" || exit 1
}

# tag FILE EXT MULTI: with several languages in the corpus, suffix each task id with `-EXT`.
tag() {
  [ "$3" = "1" ] || return 0
  python3 - "$1" "$2" <<'PY' || exit 1
import json
import sys
from pathlib import Path

path, ext = Path(sys.argv[1]), sys.argv[2]
lines = []
for line in path.read_text().splitlines():
    task = json.loads(line)
    task["id"] = f"{task['id']}-{ext}"
    lines.append(json.dumps(task, sort_keys=True, ensure_ascii=False))
path.write_text("".join(f"{line}\n" for line in lines))
PY
}

# structural REPO LANGUAGE EXT TMP MULTI: the kinds that need the language's own gold; prints the files.
structural() {
  local repo="$1" language="$2" ext="$3" tmp="$4" multi="$5" files=()
  case "${language}" in
  rust)
    gen gen_rust.py "${repo}" "${tmp}/rust-${ext}.jsonl"
    gen gen_git.py "${repo}" "${tmp}/git-${ext}.jsonl"
    files+=("${tmp}/rust-${ext}.jsonl" "${tmp}/git-${ext}.jsonl")
    ;;
  python)
    gen gen_symbols.py "${repo}" "${tmp}/symbols-${ext}.jsonl" --ext py
    gen gen_outline.py "${repo}" "${tmp}/outline-${ext}.jsonl" --ext py
    gen gen_references.py "${repo}" "${tmp}/references-${ext}.jsonl" --ext py --mode both
    gen gen_dependents.py "${repo}" "${tmp}/dependents-${ext}.jsonl" --ext py
    gen gen_git.py "${repo}" "${tmp}/git-${ext}.jsonl" --ext "${ext}" --kind git_path --kind blame --kind diff
    files+=("${tmp}/symbols-${ext}.jsonl" "${tmp}/outline-${ext}.jsonl" "${tmp}/references-${ext}.jsonl"
      "${tmp}/dependents-${ext}.jsonl" "${tmp}/git-${ext}.jsonl")
    ;;
  typescript | go | java)
    local kind
    while IFS= read -r kind; do
      gen gen_structural.py "${repo}" "${tmp}/${kind}-${ext}.jsonl" --lang "${language}" --kind "${kind}"
      files+=("${tmp}/${kind}-${ext}.jsonl")
    done < <(python3 "${here}/gen_structural.py" --lang "${language}" --list-kinds)
    gen gen_git.py "${repo}" "${tmp}/git-${ext}.jsonl" --ext "${ext}" --kind git_path --kind blame --kind diff
    files+=("${tmp}/git-${ext}.jsonl")
    ;;
  esac
  gen gen_grep.py "${repo}" "${tmp}/grep-${ext}.jsonl" --ext "${ext}"
  files+=("${tmp}/grep-${ext}.jsonl")
  local f
  for f in "${files[@]}"; do tag "${f}" "${ext}" "${multi}"; done
  printf '%s\n' "${files[@]}"
}

# analysis REPO LANGUAGE EXT TMP MULTI: the 0.34 analysis kinds; prints the files.
analysis() {
  local repo="$1" language="$2" ext="$3" tmp="$4" multi="$5" files=() gen_name
  case "${language}" in
  rust | python | typescript)
    local graph_ext="${ext}"
    [ "${language}" = typescript ] && graph_ext="ts,tsx"
    gen gen_graph.py "${repo}" "${tmp}/graph-${ext}.jsonl" --ext "${graph_ext}"
    files+=("${tmp}/graph-${ext}.jsonl")
    ;;
  esac
  for gen_name in changed large read_around; do
    gen "gen_${gen_name}.py" "${repo}" "${tmp}/${gen_name}-${ext}.jsonl" --ext "${ext}"
    files+=("${tmp}/${gen_name}-${ext}.jsonl")
  done
  if [ "${language}" = rust ]; then
    gen gen_explain.py "${repo}" "${tmp}/explain-${ext}.jsonl" --ext rs
    files+=("${tmp}/explain-${ext}.jsonl")
  fi
  local f
  for f in "${files[@]}"; do tag "${f}" "${ext}" "${multi}"; done
  printf '%s\n' "${files[@]}"
}

while IFS=$'\t' read -r name _ commit _ _ languages; do
  case "${name}" in '' | '#'*) continue ;; esac
  repo="${corpus}/${name}"
  head="$(git -C "${repo}" rev-parse HEAD)"
  if [ "${head}" != "${commit}" ] || [ -n "$(git -C "${repo}" status --porcelain)" ]; then
    echo "${name}: not a clean checkout of ${commit}; run fetch_corpus.sh first" >&2
    exit 1
  fi
  IFS=',' read -r -a langs <<<"${languages:?${name}: corpus.tsv has no languages column}"
  multi=0
  [ "${#langs[@]}" -gt 1 ] && multi=1
  tmp="$(mktemp -d)"
  parts=()
  for language in "${langs[@]}"; do
    ext="$(ext_of "${language}")"
    listed="$(structural "${repo}" "${language}" "${ext}" "${tmp}" "${multi}")"
    while IFS= read -r f; do parts+=("${f}"); done <<<"${listed}"
  done
  gen gen_find.py "${repo}" "${tmp}/find.jsonl" --ext ""
  gen gen_git_search.py "${repo}" "${tmp}/git_search.jsonl" --ext ""
  parts+=("${tmp}/find.jsonl" "${tmp}/git_search.jsonl")
  for language in "${langs[@]}"; do
    ext="$(ext_of "${language}")"
    listed="$(analysis "${repo}" "${language}" "${ext}" "${tmp}" "${multi}")"
    while IFS= read -r f; do parts+=("${f}"); done <<<"${listed}"
  done
  {
    echo "# value-gate tasks for ${name} at ${commit}, generated by gen_tasks.sh (seed ${seed}, n ${n})"
    cat "${parts[@]}"
  } >"${out}/${name}.jsonl"
  rm -rf "${tmp}"
  echo "${name}: $(grep -vc '^#' "${out}/${name}.jsonl") tasks -> ${out}/${name}.jsonl"
done <"${here}/corpus.tsv"
