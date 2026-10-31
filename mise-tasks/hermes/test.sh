#!/usr/bin/env bash
#MISE description="Validate and build the generated Hermes plugin package"
set -euo pipefail

uv run --no-project --with pytest --with mcp==1.28.1 python -m pytest pip-package-hermes/tests

wheel_dir="$(mktemp -d)"
trap 'rm -rf "$wheel_dir"' EXIT
uv build --wheel --out-dir "$wheel_dir" plugin/.hermes/package
