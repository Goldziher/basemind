#!/usr/bin/env bash

set -euo pipefail

ROOT="${BASEMIND_HARDEN_ROOT:-/tmp/basemind-harden}"
RESULTS="${ROOT}/results.ndjson"
FEATURES="${BASEMIND_HARDEN_FEATURES-full}"
feature_args=()
if [ -n "${FEATURES}" ]; then feature_args=(--features "${FEATURES}"); fi
mkdir -p "${ROOT}"
: >"${RESULTS}"

# Isolate the index cache AND any comms daemon a `full` (comms) build spawns to a harness-owned
# directory, so a hardening run never writes 8 giant OSS repos into the developer's live global
# cache (~/Library/Application Support/basemind) or collides with their running daemon. The index no
# longer lives in each repo's `.basemind/` — it is machine-global under `BASEMIND_DATA_HOME`, keyed
# by workspace path — so a clean run wipes THAT, not a per-repo directory. Both are overridable.
export BASEMIND_DATA_HOME="${BASEMIND_DATA_HOME:-${ROOT}/cache}"
export BASEMIND_COMMS_DIR="${BASEMIND_COMMS_DIR:-${ROOT}/comms}"
if [ -z "${BASEMIND_HARDEN_KEEP:-}" ]; then
	echo "==> wiping prior global index cache at ${BASEMIND_DATA_HOME}"
	rm -rf "${BASEMIND_DATA_HOME}"
fi

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
