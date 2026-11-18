#!/usr/bin/env bash

set -euo pipefail

ROOT="${BASEMIND_HARDEN_ROOT:-/tmp/basemind-harden}"
RESULTS="${ROOT}/results.ndjson"
FEATURES="${BASEMIND_HARDEN_FEATURES-full}"
feature_args=()
if [ -n "${FEATURES}" ]; then feature_args=(--features "${FEATURES}"); fi
mkdir -p "${ROOT}"
: >"${RESULTS}"

export BASEMIND_DATA_HOME="${BASEMIND_DATA_HOME:-${ROOT}/cache}"
export BASEMIND_COMMS_DIR="${BASEMIND_COMMS_DIR:-${ROOT}/comms}"
# Grammars live under the data home by default; keep them out of the wipe below so a cold-cache
# run measures the scan, not the grammar download.
export TREE_SITTER_LANGUAGE_PACK_CACHE_DIR="${TREE_SITTER_LANGUAGE_PACK_CACHE_DIR:-${ROOT}/grammars}"
if [ -z "${BASEMIND_HARDEN_KEEP:-}" ]; then
  echo "==> wiping prior global index cache at ${BASEMIND_DATA_HOME}"
  rm -rf "${BASEMIND_DATA_HOME}"
fi

# name|url|commit|depth. Each canary repo is pinned to a commit, so an upstream push cannot move a
# canary or turn the nightly red on its own; move a pin deliberately, in its own commit. An empty
# depth fetches the full history.
REPOS=(
  "ripgrep|https://github.com/BurntSushi/ripgrep.git|178cc027e283fd965cb72d0a7395b219eada170c|"
  "tokio|https://github.com/tokio-rs/tokio.git|f3e855fa55d6c1cc3e7a28e115a421b8ee5860bd|2000"
  "typescript|https://github.com/microsoft/TypeScript.git|68672025cc6f01c845051d9f9b04200b0382942c|2000"
  "react|https://github.com/facebook/react.git|1cd5eb8e0edb0b3a1ba33de9c886d91627f8b5ac|2000"
  "django|https://github.com/django/django.git|d3954c981368eb4439be63f2f165d400f33bcd70|2000"
  "requests|https://github.com/psf/requests.git|3c7baaa05ab4aa49cc819ce3a819d260de19c157|"
  "gin|https://github.com/gin-gonic/gin.git|0f09c3a9b4626d9fe9979cf4d0521d6ae32ab646|"
  "ripgrep-shallow|https://github.com/BurntSushi/ripgrep.git|178cc027e283fd965cb72d0a7395b219eada170c|50"
)

selected=("$@")
should_run() {
  local name="$1"
  if [ "${#selected[@]}" -eq 0 ]; then return 0; fi
  for s in "${selected[@]}"; do [ "${s}" = "${name}" ] && return 0; done
  return 1
}

if [ -z "${BASEMIND_HARDEN_NO_BUILD:-}" ]; then
  echo "==> building basemind (release, features: ${FEATURES:-default})"
  cargo build --release --quiet ${feature_args[@]+"${feature_args[@]}"} --bin basemind
fi

workspace_root="$(pwd -P)"
target_dir="${CARGO_TARGET_DIR:-${workspace_root}/target}"
if [[ "${target_dir}" != /* ]]; then target_dir="${workspace_root}/${target_dir}"; fi
export BASEMIND_DAEMON_BINARY="${BASEMIND_DAEMON_BINARY:-${target_dir}/release/basemind}"

failed=()
passed=()

for entry in "${REPOS[@]}"; do
  IFS='|' read -r name url commit depth <<<"${entry}"
  should_run "${name}" || continue

  dest="${ROOT}/${name}"
  echo
  echo "================================================================"
  echo "== ${name}"
  echo "================================================================"

  if [ "$(git -C "${dest}" rev-parse HEAD 2>/dev/null || true)" != "${commit}" ]; then
    echo "==> fetching ${url} at ${commit}${depth:+ (depth ${depth})} → ${dest}"
    rm -rf "${dest}"
    git init -q "${dest}"
    git -C "${dest}" remote add origin "${url}"
    git -C "${dest}" fetch -q ${depth:+--depth="${depth}"} origin "${commit}"
    git -C "${dest}" checkout -q --detach FETCH_HEAD
    [ "$(git -C "${dest}" rev-parse HEAD)" = "${commit}" ] || {
      echo "error: ${dest} is not at the pinned ${commit}" >&2
      exit 1
    }
  else
    echo "==> reusing the clone at ${dest} (${commit})"
  fi

  ok=1
  if [ -z "${BASEMIND_HARDEN_SKIP_SUITE:-}" ]; then
    BASEMIND_HARDEN_REPO="${dest}" \
      BASEMIND_HARDEN_REPO_NAME="${name}" \
      BASEMIND_HARDEN_RESULTS="${RESULTS}" \
      cargo test --release ${feature_args[@]+"${feature_args[@]}"} --test harden -- \
      --ignored --nocapture --test-threads=1 --exact harden_repo || ok=0
  fi

  # BASEMIND_HARDEN_WORKTREES=<K> adds K git worktrees at older commits and measures what each
  # costs on disk and to first-scan. BASEMIND_HARDEN_SKIP_SUITE=1 runs that measurement alone.
  if [ -n "${BASEMIND_HARDEN_WORKTREES:-}" ]; then
    BASEMIND_HARDEN_REPO="${dest}" \
      BASEMIND_HARDEN_REPO_NAME="${name}" \
      BASEMIND_HARDEN_RESULTS="${RESULTS}" \
      cargo test --release ${feature_args[@]+"${feature_args[@]}"} --test harden -- \
      --ignored --nocapture --test-threads=1 --exact worktrees::harden_worktrees || ok=0
  fi

  if [ "${ok}" -eq 1 ]; then
    passed+=("${name}")
  else
    failed+=("${name}")
  fi
done

echo
echo "================================================================"
echo "== summary"
echo "================================================================"
echo "results: ${RESULTS}"
echo "passed (${#passed[@]}): ${passed[*]:-<none>}"
echo "failed (${#failed[@]}): ${failed[*]:-<none>}"

if [ "${#failed[@]}" -gt 0 ]; then
  exit 1
fi
