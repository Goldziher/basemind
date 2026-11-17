---
status: proposed
date: 2026-11-17
amends: ["0027"]
---

# ADR-0033: A dated marker for plugin content ahead of its release

- **Deciders:** the repository owner
- **Related:** amends ADR-0027 (rule 4 and rule 5); ADR-0018, ADR-0026

## Context

The Claude and Codex marketplaces serve the plugin from `./plugin` on the default branch, and the
plugin manifests pin the binary the launcher installs. ADR-0027 rule 4 lets plugin content land on
`main` with the code that serves it while the manifests keep pinning the last release, and checks
the content only against the binary built from the same commit. So a user who installs or updates
the plugin from `main` runs content that names tools the pinned binary lacks: on `main` at
`692ffdb5`, 65 of 154 tool, mode, argument and CLI names the content uses are not served by 0.33.0.

A check against the tree build alone cannot see that, and a check against the pinned release fails
for as long as ADR-0027 rule 4 allows the state. Without a record of the state, either the check is
not run or it is permanently red.

## Decision

1. `tests/plugin_tool_skew.rs` checks the plugin content twice: against the tree build, started
   through the launcher, and against the release the manifests pin (`BASEMIND_SKEW_PINNED_BIN`; CI
   downloads it, or uses the tree build when the pinned version has no release and is the tree's
   own version).
2. Main may name a surface the pinned release lacks only while `.github/unreleased-surface.toml`
   declares it, with `pinned_version`, `since`, `review_by` (both `YYYY-MM-DD`) and `reason`.
   While it exists the pinned check prints every unserved name instead of failing.
3. The marker is not a waiver that can be forgotten. The pinned check fails when the marker names
   another pinned version than the manifests, when the pinned release serves everything the
   content names (the marker is stale), and after `review_by`. Renewing `review_by` is a commit
   that states the decision again.
4. A release ends the state. The release commit removes the marker, and the `meta` job of
   `publish.yaml` refuses to publish while it exists.
5. The marketplaces keep serving `./plugin` from the default branch.

## Consequences

- The skew ADR-0027 accepted is visible on every CI run, with the list of names a user of `main`
  cannot call, and it cannot outlive a release or its review date unnoticed.
- `main` goes red on `review_by` unless someone releases or renews the marker; that is the point.
- CI downloads the pinned release on the legs that build `comms` with `documents` (Linux and macOS);
  Windows runs only the tree-build check.
- Names the tree build also lacks are listed in the test's `KNOWN_MISMATCHES` and reported there,
  not again by the pinned check.

## Alternatives considered

- **Pin the marketplaces to a release ref** — ends the skew for the Claude and Codex marketplaces,
  but the index is ai-rulez output and other hosts install from other channels; not chosen by the
  owner.
- **Check only the tree build (ADR-0027 rule 4 as written)** — passes while users run content their
  binary cannot serve.
- **Fail the pinned check whenever it finds a mismatch** — red on every commit between a surface
  change and its release, so it would be ignored or switched off.
