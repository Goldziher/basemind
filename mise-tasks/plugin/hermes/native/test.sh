#!/usr/bin/env bash
#MISE description="Verify required registration using the installed Hermes SDK"
set -euo pipefail

hermes_python="${HERMES_PYTHON:-python3}"
if ! "$hermes_python" -c "import hermes_cli.plugins"; then
  echo "Set HERMES_PYTHON to the Python interpreter containing the Hermes Agent SDK." >&2
  exit 1
fi
"$hermes_python" scripts/tests/native_hermes_registration.py
