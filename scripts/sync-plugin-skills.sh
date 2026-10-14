#!/usr/bin/env bash

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

# Derived from the canonical trees rather than listed, so adding a skill or command ships it
# everywhere instead of silently reaching only the trees someone remembered to update. The
# hand-maintained list had drifted to 9 of 10 skills: multi-agent-room existed in skills/ and was
# referenced by the shipped agent-comms rule, but was absent from every plugin bundle that rule
# ships into, so the agent was told to consult a skill its host did not have. ~keep
AI_RULEZ="${AI_RULEZ:-ai-rulez}"
"$AI_RULEZ" generate --plugin --no-local --strict-config
python3 scripts/sync_public_skills.py
python3 scripts/sync_legacy_plugin_exports.py
