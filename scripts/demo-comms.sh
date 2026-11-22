#!/usr/bin/env bash
set -euo pipefail

# Two agents coordinate through `basemind agent`. Run from a repository
# checkout with BASEMIND_COMMS_DIR pointing at a throwaway directory.
export CLICOLOR_FORCE=1
PROMPT="$(printf '\033[1;32m❯\033[0m ')"

OUT=""
pe() {
  printf '%s%s\n' "$PROMPT" "$1"
  OUT="$(eval "$1" || true)"
  printf '%s\n\n' "$OUT"
  sleep "${DEMO_PAUSE:-2.5}"
}

pe "BASEMIND_AGENT_ID=orchestrator basemind agent post --subject 'review: session adapters' --paths src/requests --body 'Reviewing sessions.py. Reviewer: take get_adapter and its callers.'"
THREAD="$(printf '%s' "$OUT" | cut -f1 | cut -d. -f1)"
pe "BASEMIND_AGENT_ID=reviewer basemind agent threads --query review"
pe "BASEMIND_AGENT_ID=reviewer basemind agent threads --thread $THREAD --join"
pe "BASEMIND_AGENT_ID=reviewer basemind agent post --thread $THREAD --body 'get_adapter has 11 refs, none broken. No findings.'"
pe "BASEMIND_AGENT_ID=orchestrator basemind agent inbox"
