#!/usr/bin/env bash
# Refuse to publish a commit that CI has not passed.
#
# usage: require-green-ci.sh <commit-sha>
#
# A tag push starts publish.yaml on its own; nothing else ties a release to the CI verdict for the
# tagged commit. This waits for the newest dispatched or scheduled `ci` workflow run on that exact
# commit to finish and then requires:
#   - its conclusion is `success` (a cancelled, failed or timed-out run is a refusal, not a retry);
#   - it ran at least one `test / ...` matrix leg, all of them successful (a run that skipped the
#     matrix would otherwise read as green);
#   - its harden harness job succeeded (ADR-0027 rule 5). A push run never has it: the job runs only
#     on dispatch and schedule, and a skipped job leaves the run's conclusion `success`;
#   - no other check run on the commit (the path-filtered AI-Rulez validation, for one) concluded
#     failure, cancelled, timed_out or action_required.
#
# Env: GITHUB_REPOSITORY (owner/repo), GH_TOKEN or GITHUB_TOKEN with actions:read and checks:read.
# CI_GATE_WAIT_MINUTES (default 240) bounds the wait for a run that is queued or in progress;
# CI_GATE_POLL_SECONDS (default 60) is the poll interval.
set -euo pipefail

log() { printf 'require-green-ci: %s\n' "$*" >&2; }
die() {
  printf '::error::require-green-ci: %s\n' "$*" >&2
  exit 1
}

[ "$#" -eq 1 ] || die "usage: $0 <commit-sha>"
SHA="$1"
[[ "$SHA" =~ ^[0-9a-f]{40}$ ]] || die "expected a full 40-character commit sha, got '$SHA'"
REPO="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY must be set (owner/repo)}"
WAIT_MINUTES="${CI_GATE_WAIT_MINUTES:-240}"
POLL_SECONDS="${CI_GATE_POLL_SECONDS:-60}"

deadline=$(($(date +%s) + WAIT_MINUTES * 60))

HARDEN_JOB="hardening harness (nightly)"

# Newest ci run on SHA as "<id> <status> <conclusion>", or nothing. Only dispatch and schedule runs
# carry the harden harness; pull_request runs test a merge commit, never SHA itself.
newest_run() {
  gh api "repos/$REPO/actions/workflows/ci.yaml/runs?head_sha=$SHA&per_page=100" \
    -q '[.workflow_runs[] | select(.event == "workflow_dispatch" or .event == "schedule")]
        | sort_by(.created_at) | last | select(. != null)
        | "\(.id) \(.status) \(.conclusion // "none")"'
}

while :; do
  run="$(newest_run)"
  if [ -n "$run" ]; then
    read -r run_id status conclusion <<<"$run"
    [ "$status" = "completed" ] && break
    log "ci run $run_id on $SHA is $status; waiting"
  else
    log "no dispatched ci run on $SHA yet; waiting (gh workflow run ci.yaml --ref <release branch>)"
  fi
  [ "$(date +%s)" -lt "$deadline" ] || die "no completed ci run on $SHA within ${WAIT_MINUTES} minutes"
  sleep "$POLL_SECONDS"
done

log "newest ci run on $SHA: $run_id ($conclusion)"
[ "$conclusion" = "success" ] ||
  die "ci run $run_id on $SHA concluded '$conclusion'; https://github.com/$REPO/actions/runs/$run_id"

jobs="$(gh api --paginate "repos/$REPO/actions/runs/$run_id/jobs?per_page=100" \
  -q '.jobs[] | "\(.conclusion // "none")\t\(.name)"')"
legs="$(printf '%s\n' "$jobs" | awk -F '\t' '$2 ~ /^test \/ /' || true)"
total="$(printf '%s\n' "$legs" | grep -c . || true)"
bad="$(printf '%s\n' "$legs" | awk -F '\t' 'NF && $1 != "success"' || true)"
[ "$total" -gt 0 ] || die "ci run $run_id ran no 'test / ...' leg, so it vouches for nothing"
[ -z "$bad" ] || die "ci run $run_id has test legs that did not succeed:
$bad"
log "ci run $run_id: $total of $total test legs succeeded"

harden="$(printf '%s\n' "$jobs" | awk -F '\t' -v job="$HARDEN_JOB" '$2 == job {print $1}')"
[ "$harden" = "success" ] ||
  die "ci run $run_id: '$HARDEN_JOB' is '${harden:-absent}', and the harden harness gates every release"
log "ci run $run_id: the harden harness succeeded"

failing="$(gh api --paginate "repos/$REPO/commits/$SHA/check-runs?per_page=100" \
  -q '.check_runs[]
      | select(.conclusion == "failure" or .conclusion == "cancelled" or .conclusion == "timed_out" or .conclusion == "action_required")
      | select(.name | test("^(Publish|Resolve versions|Require a green CI)") | not)
      | "\(.conclusion)\t\(.name)"')"
[ -z "$failing" ] || die "check runs on $SHA did not pass:
$failing"
log "no failing check run on $SHA"
