#!/usr/bin/env bash
#MISE description="Stage and pack the combined OpenCode and Pi package without changing the checkout"
#MISE alias="package:pi"
set -euo pipefail

package_dir="$(mktemp -d)"
trap 'rm -rf "$package_dir"' EXIT
bash scripts/stage-agent-package.sh "$package_dir"
pnpm --dir "$package_dir" pack --pack-destination "$package_dir"
