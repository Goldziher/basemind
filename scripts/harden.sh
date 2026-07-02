#!/usr/bin/env bash
# Real-OSS hardening harness orchestrator.
#
# Clones a diverse set of upstream repos into /tmp/basemind-harden/, then runs
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
#   BASEMIND_HARDEN_ROOT     base dir for clones (default /tmp/basemind-harden)
#   BASEMIND_HARDEN_KEEP=1   keep .basemind/ from prior runs (default: wipe between repos)
#   BASEMIND_HARDEN_NO_BUILD=1   skip the up-front `cargo build --release`
#   BASEMIND_HARDEN_FEATURES cargo feature set to build/run with (default "full"). Set to "" to
#                            build default features only — useful on machines where the optional
#                            documents/memory/intelligence stack can't compile (the harness records
#                            those tools as skipped rather than failing). Scan + git-ops are
#                            measured on whatever feature set is given.

set -euo pipefail

ROOT="${BASEMIND_HARDEN_ROOT:-/tmp/basemind-harden}"
RESULTS="${ROOT}/results.ndjson"
FEATURES="${BASEMIND_HARDEN_FEATURES-full}"
feature_args=()
if [ -n "${FEATURES}" ]; then feature_args=(--features "${FEATURES}"); fi
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

if [ -z "${BASEMIND_HARDEN_NO_BUILD:-}" ]; then
	echo "==> building basemind (release, features: ${FEATURES:-default})"
	cargo build --release --quiet ${feature_args[@]+"${feature_args[@]}"} --bin basemind
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

	if [ -z "${BASEMIND_HARDEN_KEEP:-}" ] && [ -d "${dest}/.basemind" ]; then
		echo "==> wiping prior .basemind/ index"
		rm -rf "${dest}/.basemind"
	fi

	# Run the harness against this repo. `--test-threads=1` keeps the per-repo
	# output legible; the test itself doesn't care about parallelism.
	if BASEMIND_HARDEN_REPO="${dest}" \
		BASEMIND_HARDEN_REPO_NAME="${name}" \
		BASEMIND_HARDEN_RESULTS="${RESULTS}" \
		cargo test --release ${feature_args[@]+"${feature_args[@]}"} --test harden -- \
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
