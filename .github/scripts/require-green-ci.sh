#!/usr/bin/env bash
# Refuse to publish a commit that CI has not passed.
#
# usage: require-green-ci.sh <commit-sha>
#
# A tag push starts publish.yaml on its own; nothing else ties a release to the CI verdict for the
# tagged commit. This waits for the newest `ci` workflow run on that exact commit to finish and then
# requires:
#   - its conclusion is `success` (a cancelled, failed or timed-out run is a refusal, not a retry);
#   - it ran at least one `test / ...` matrix leg, all of them successful (a run that skipped the
#     matrix would otherwise read as green);
#   - no other check run on the commit (the path-filtered AI-Rulez validation, for one) concluded
#     failure, cancelled, timed_out or action_required;
#   - the release gates a push run skips (ADR-0027 rule 5) passed on that commit: a scheduled or
#     dispatched `ci` run whose nightly jobs (the harden harness, the ignored tests and every leg of
#     the release build) all succeeded, and a scheduled or dispatched `hardening` run that succeeded.
#     A run still in progress is waited for; when there is none, dispatch both on the commit.
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

# Newest run of workflow $1 on SHA among the events matching the regex $2, as
# "<id> <status> <conclusion>", or nothing. Push, dispatch and schedule runs all test the same tree;
# pull_request runs test a merge commit, never SHA itself. A push run counts only on main: a push to
# any other branch runs the fast legs alone, so it vouches for less than a run on main.
newest_run() {
  gh api "repos/$REPO/actions/workflows/$1/runs?head_sha=$SHA&per_page=100" \
    -q '[.workflow_runs[] | select((.event | test("^('"$2"')$")) and (.event != "push" or .head_branch == "main"))]
        | sort_by(.created_at) | last | select(. != null)
        | "\(.id) \(.status) \(.conclusion // "none")"'
}

# "<conclusion>\t<name>" for every job of run $1.
job_conclusions() {
  gh api --paginate "repos/$REPO/actions/runs/$1/jobs?per_page=100" -q '.jobs[] | "\(.conclusion // "none")\t\(.name)"'
}

# Wait for the newest scheduled or dispatched run of workflow $1 on SHA to complete and require it
# to have succeeded; print its id. Refuse at once when there is no such run.
require_release_run() {
  local workflow="$1" run run_id status conclusion
  while :; do
    run="$(newest_run "$workflow" "schedule|workflow_dispatch")"
    [ -n "$run" ] ||
      die "no scheduled or dispatched $workflow run on $SHA, which the release gates need; dispatch one: gh workflow run $workflow --ref <branch at $SHA>"
    read -r run_id status conclusion <<<"$run"
    [ "$status" = "completed" ] && break
    log "$workflow run $run_id on $SHA is $status; waiting"
    [ "$(date +%s)" -lt "$deadline" ] || die "$workflow run $run_id on $SHA did not complete within ${WAIT_MINUTES} minutes"
    sleep "$POLL_SECONDS"
  done
  [ "$conclusion" = "success" ] ||
    die "$workflow run $run_id on $SHA concluded '$conclusion'; https://github.com/$REPO/actions/runs/$run_id"
  printf '%s\n' "$run_id"
}

while :; do
  run="$(newest_run ci.yaml "push|workflow_dispatch|schedule")"
  if [ -n "$run" ]; then
    read -r run_id status conclusion <<<"$run"
    [ "$status" = "completed" ] && break
    log "ci run $run_id on $SHA is $status; waiting"
  else
    log "no ci run on $SHA yet; waiting (push the commit to main, or dispatch ci on it)"
  fi
  [ "$(date +%s)" -lt "$deadline" ] || die "no completed ci run on $SHA within ${WAIT_MINUTES} minutes"
  sleep "$POLL_SECONDS"
done

log "newest ci run on $SHA: $run_id ($conclusion)"
[ "$conclusion" = "success" ] ||
  die "ci run $run_id on $SHA concluded '$conclusion'; https://github.com/$REPO/actions/runs/$run_id"

legs="$(job_conclusions "$run_id" | awk -F '\t' 'index($2, "test / ") == 1')"
total="$(printf '%s\n' "$legs" | grep -c . || true)"
bad="$(printf '%s\n' "$legs" | awk -F '\t' 'NF && $1 != "success"' || true)"
[ "$total" -gt 0 ] || die "ci run $run_id ran no 'test / ...' leg, so it vouches for nothing"
[ -z "$bad" ] || die "ci run $run_id has test legs that did not succeed:
$bad"
log "ci run $run_id: $total of $total test legs succeeded"

# The jobs of publish.yaml itself are not CI's verdict on the commit: a failed earlier attempt of
# the release (a re-run or a dispatch for the same tag) leaves its failed jobs on the commit, and
# counting them would refuse every retry. Their names come from the workflow, so a new job needs
# no edit here.
publish_jobs="$(sed -n 's/^    name: //p' "$(dirname "${BASH_SOURCE[0]}")/../workflows/publish.yaml" | jq -R . | jq -s .)"
[ "$(jq length <<<"$publish_jobs")" -gt 0 ] || die "found no job names in publish.yaml"
failing="$(gh api --paginate "repos/$REPO/commits/$SHA/check-runs?per_page=100" |
  jq -r --argjson own "$publish_jobs" '.check_runs[]
      | select(.conclusion == "failure" or .conclusion == "cancelled" or .conclusion == "timed_out" or .conclusion == "action_required")
      | select(.name as $n | $own | index($n) | not)
      | "\(.conclusion)\t\(.name)"')"
[ -z "$failing" ] || die "check runs on $SHA did not pass:
$failing"
log "no failing check run on $SHA"

# A push run skips these jobs, and a skipped job leaves a run green: each must have succeeded.
nightly_id="$(require_release_run ci.yaml)"
nightly="$(job_conclusions "$nightly_id")"
for job in "hardening harness (nightly)" "ignored tests (nightly)"; do
  printf '%s\n' "$nightly" | grep -qxF "success"$'\t'"$job" ||
    die "ci run $nightly_id did not run '$job' to success; https://github.com/$REPO/actions/runs/$nightly_id"
done
mirror="$(printf '%s\n' "$nightly" | awk -F '\t' 'index($2, "release build (nightly) / ") == 1')"
mirror_total="$(printf '%s\n' "$mirror" | grep -c . || true)"
[ "$mirror_total" -gt 0 ] || die "ci run $nightly_id ran no 'release build (nightly) / ...' job"
mirror_bad="$(printf '%s\n' "$mirror" | awk -F '\t' 'NF && $1 != "success"' || true)"
[ -z "$mirror_bad" ] || die "ci run $nightly_id has release build jobs that did not succeed:
$mirror_bad"
log "ci run $nightly_id: harden harness, ignored tests and $mirror_total release build jobs succeeded"

hardening_id="$(require_release_run hardening.yaml)"
log "hardening run $hardening_id on $SHA succeeded"
