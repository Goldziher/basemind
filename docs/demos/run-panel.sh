#!/usr/bin/env bash
# Code-review panel demo driver — see multi-agent-review.md for the narrative.
#
# Drives three named reviewers (security / perf / correctness) through one shared
# room plus pairwise DMs, then synthesizes a verdict — entirely over the comms CLI.
# Each `basemind comms` call is a separate process; the per-user comms daemon
# (auto-started on first call) persists room + inbox state across them.
#
# Record it:   asciinema rec panel.cast -c ./docs/demos/run-panel.sh
# Render GIF:  agg panel.cast panel.gif
set -euo pipefail

BM="${BM:-./target/release/basemind}"
ROOM="code-review-panel"
PAUSE="${PAUSE:-1.4}" # seconds between steps; bump for a slower recording

step() {
  printf '\n\033[1;36m▸ %s\033[0m\n' "$1"
  sleep "$PAUSE"
}
run() {
  printf '\033[2m$ %s\033[0m\n' "$*"
  "$@"
  sleep "$PAUSE"
}

step "1. Orchestrator opens the review room (session-scoped)"
run "$BM" comms room-create "$ROOM" --scope session --title "Code Review: Parallel Panel"

step "2. Three reviewers register"
run "$BM" comms register --as-agent security --name "Security Reviewer"
run "$BM" comms register --as-agent perf --name "Performance Reviewer"
run "$BM" comms register --as-agent correctness --name "Correctness Reviewer"

step "3. Reviewers post findings to the shared room"
run "$BM" comms post "$ROOM" "SQL injection risk in user lookup" --as-agent security \
  --body "Line 42 concatenates user input into a query. Use parameterized queries."
run "$BM" comms post "$ROOM" "N+1 query in user search loop" --as-agent perf \
  --body "Line 56 queries the DB inside a loop over users. Batch fetch instead."
run "$BM" comms post "$ROOM" "Null check missing on user ID" --as-agent correctness \
  --body "Line 38 assumes user_id is never null, but the API allows it. Add a guard."

step "4. Cross-check via direct messages"
run "$BM" comms dm security --to-agent security --as-agent correctness \
  --subject "Re: SQL fix + null safety" \
  --body "Your parameterized fix is solid. Put the null guard before the query."
run "$BM" comms dm security --to-agent security --as-agent perf \
  --subject "Re: SQL fix impact on latency" \
  --body "Parameterized adds ~0.1ms. The batching fix is the real win."

step "5. Orchestrator scans the room (front-matter only, recency-aware)"
run "$BM" comms history "$ROOM"

step "6. Security reviewer reads its inbox (the two peer DMs)"
run "$BM" comms inbox --as-agent security

step "7. Orchestrator posts the synthesized verdict"
run "$BM" comms post "$ROOM" "Verdict: Approve with fixes" \
  --body "SECURITY: parameterized queries. PERF: batch fetch. CORRECTNESS: null guard. Approved."

step "8. Final room state"
run "$BM" comms history "$ROOM"
