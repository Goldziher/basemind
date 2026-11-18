---
status: proposed
date: 2026-11-18
amends: ["0027"]
---

# ADR-0045: The release commit reaches main only after its release is public

- **Deciders:** the repository owner
- **Related:** amends ADR-0027 (retained rules 1 and 2, and how rule 5's harden gate is enforced);
  ADR-0035 (the marketplaces serve `./plugin` from `main`)

## Context

The Claude and Codex marketplaces serve the plugin from `./plugin` on `main` (ADR-0035 rule 5). The
plugin manifests pin the binary the launcher downloads (`scripts/mcp-launch.sh`). So as soon as the
release commit, with the manifests at the new version, is on `main`, every host that updates its
plugin pins a release that does not exist yet.

Under ADR-0027 that commit lands on `main` first: `publish.yaml`'s `ci_gate` needs a green `ci` run
on it, and the push run on `main` was that run. The push run takes about two hours, and building,
smoke-testing and promoting the release up to three more. Through that window the launcher's
fallback (`fallback_or_die`) finds no release of the pinned minor and dies, telling the user to
report a broken release, and `session-start` reports basemind unavailable.

The same order raced the CI skew gate: a tag pushed with the commit made the tag page answer 200
before the release was public, which failed the CI run the publish was waiting on.

ADR-0027 rule 5 also makes the harden harness a release gate, but `ci.yaml` runs it only on
dispatched and scheduled runs, and `ci_gate` accepted a push run, so nothing enforced it.

## Decision

1. **A release is cut from a release branch.** The release commit (`mise run release:sync-version
   X.Y.Z`, marker removed, changelog) is pushed to a branch `release/vX.Y.Z` cut from `main`, never
   to `main`.
2. **CI for the release is a dispatched run** on that branch (`gh workflow run ci.yaml --ref
   release/vX.Y.Z`). `ci_gate` (`.github/scripts/require-green-ci.sh`) accepts only a dispatched or
   scheduled run on the tagged commit, and requires its `hardening harness (nightly)` job to have
   succeeded. That enforces ADR-0027 rule 5's harden gate.
3. **Tag after green.** The owner tags the release commit `vX.Y.Z` and pushes the tag once that run
   is green.
4. **`publish.yaml` moves `main`.** After `finalize_release` has made the release public, the
   `advance_main` job fast-forwards `main` to the tagged commit and dispatches `ci` on `main`. If
   `main` moved meanwhile and cannot fast-forward, the job fails and the owner merges the tag into
   `main` by hand. Nothing else lands on `main` while a release is in flight.

This amends ADR-0027's retained rule 1 (maintainers fast-forward their branches onto `main`): the
release commit is the one commit the publish workflow fast-forwards. It amends retained rule 2 (CI
runs on `main` after every push): a push made with the workflow token starts no workflow, so
`advance_main` dispatches the run.

## Consequences

- No host pins a release before its assets are public, so the launcher's fallback is no longer
  reached on release day.
- The release branch lives for the hours between the version bump and publication, the only branch
  besides short-lived work branches.
- A release costs one dispatched CI run, which includes the harden harness, the nightly release
  build and the ignored tests: longer than a push run. A push run no longer vouches for a release.
- `advance_main` needs `contents: write` on `main`. If `main` is protected against the workflow
  token, the job fails after the release is public and the owner fast-forwards `main` by hand; the
  release itself is unaffected.
- A rerun of the publish workflow finds `main` already containing the tag and skips the push.

## Alternatives considered

- **Keep landing the release commit on `main` first and document the window** — rejected: every
  auto-updating host loses its MCP server for hours on every release.
- **Pin the marketplaces to a release ref** — the marketplaces would never run ahead of a release,
  but the marketplace index is ai-rulez output and other hosts install from other channels; ADR-0035
  left it unchosen for that reason.
- **Let the launcher fall back across minors during the window** — rejected: a minor changes the
  tools and formats, and ADR-0027 rule 4 holds plugin content to the binary that serves it.
