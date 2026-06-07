#!/usr/bin/env bash
# Real-OSS hardening harness orchestrator.
#
# Clones a diverse set of upstream repos into /tmp/gitmind-harden/, then runs
# `tests/harden.rs` against each one. The harness exits non-zero if any repo
# trips its per-repo or generic assertions.
#
# This is the gating artifact for the hardening iteration — every stage's
# success is judged by whether it monotonically reduces this harness's failures.
#
# Run:
#   ./scripts/harden.sh                 # all repos
#   ./scripts/harden.sh react ripgrep   # subset (logical names below)
#
# Env overrides:
#   GITMIND_HARDEN_ROOT     base dir for clones (default /tmp/gitmind-harden)
#   GITMIND_HARDEN_KEEP=1   keep .gitmind/ from prior runs (default: wipe between repos)
#   GITMIND_HARDEN_NO_BUILD=1   skip the up-front `cargo build --release`

set -euo pipefail

ROOT="${GITMIND_HARDEN_ROOT:-/tmp/gitmind-harden}"
RESULTS="${ROOT}/results.ndjson"
mkdir -p "${ROOT}"
: >"${RESULTS}"

# Repo set. Format: "logical_name git_url extra_clone_args"
# Logical names are matched by tests/harden.rs for repo-specific canaries:
#   - react           — TSX + JSX (useState canary)
#   - ripgrep-shallow — shallow-clone detection (truncated canary)
REPOS=(
  "ripgrep|https://github.com/BurntSushi/ripgrep.git|"
  "tokio|https://github.com/tokio-rs/tokio.git|--depth=2000"
  "typescript|https://github.com/microsoft/TypeScript.git|--depth=2000"
  "react|https://github.com/facebook/react.git|--depth=2000"
  "django|https://github.com/django/django.git|--depth=2000"
  "requests|https://github.com/psf/requests.git|"
  "gin|https://github.com/gin-gonic/gin.git|"
  "ripgrep-shallow|https://github.com/BurntSushi/ripgrep.git|--depth=50"
)

selected=("$@")
should_run() {
  local name="$1"
  if [ "${#selected[@]}" -eq 0 ]; then return 0; fi
  for s in "${selected[@]}"; do [ "${s}" = "${name}" ] && return 0; done
  return 1
}

if [ -z "${GITMIND_HARDEN_NO_BUILD:-}" ]; then
  echo "==> building gitmind (release)"
  cargo build --release --quiet --bin gitmind
fi

# Track overall outcome
failed=()
passed=()

for entry in "${REPOS[@]}"; do
  IFS='|' read -r name url extra <<<"${entry}"
  should_run "${name}" || continue

  dest="${ROOT}/${name}"
  echo
  echo "================================================================"
  echo "== ${name}"
  echo "================================================================"

  if [ ! -d "${dest}/.git" ]; then
    echo "==> cloning ${url} → ${dest}"
    # shellcheck disable=SC2086
    git clone ${extra} "${url}" "${dest}"
  else
    echo "==> reusing existing clone at ${dest}"
  fi

  if [ -z "${GITMIND_HARDEN_KEEP:-}" ] && [ -d "${dest}/.gitmind" ]; then
    echo "==> wiping prior .gitmind/ index"
    rm -rf "${dest}/.gitmind"
  fi

  # Run the harness against this repo. `--test-threads=1` keeps the per-repo
  # output legible; the test itself doesn't care about parallelism.
  if GITMIND_HARDEN_REPO="${dest}" \
    GITMIND_HARDEN_REPO_NAME="${name}" \
    GITMIND_HARDEN_RESULTS="${RESULTS}" \
    cargo test --release --test harden -- \
    --ignored --nocapture --test-threads=1 --exact harden_repo; then
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
