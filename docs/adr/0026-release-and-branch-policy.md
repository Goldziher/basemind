---
status: accepted
date: 2026-11-15
supersedes: "0010"
---

# ADR-0026: Release and branch policy — trunk-based, three breaking releases

- **Deciders:** basemind maintainers
- **Related:** supersedes ADR-0010; ADR-0017, ADR-0021, ADR-0023, ADR-0024, ADR-0025

## Context

ADR-0010 is the standing branch and release policy. Most of it describes work that no longer
exists:

- It routes UI work to a long-lived `feat/agent-layer` branch kept rebased on `main`. The UI it was
  for is gone: ADR-0006 is withdrawn, and ADR-0017 superseded ADR-0007 and deleted the graph,
  rendering and document-link roadmap (ADR-0001 to ADR-0005, ADR-0008, ADR-0009).
- Its cherry-pick mechanism, its UI feature gate, the desktop app in the release archive and the
  optional docker image all concern that branch and that UI.

One clause is still in force: versions are bumped in lockstep through the single sanctioned
workflow.

Since then five accepted ADRs each carry a breaking change: the verb-tool surface (ADR-0018), agent
comms v2 (ADR-0021), daemon generations (ADR-0024), store versioning (ADR-0025) and the crate split
(ADR-0023). Nothing says in which release each lands, and each release that changes an on-disk
format costs every user a rescan.

The plugin adds a constraint that ADR-0010 never considered. The marketplace entry in
`.claude-plugin/marketplace.json` has `"source": "./plugin"`: a path inside this repository, so a
host that installs or updates the plugin takes its hooks, skills and commands from whatever commit
of the default branch it fetched. The binary is pinned separately: the launcher
(`scripts/mcp-launch.sh`) reads the version from the plugin manifest and downloads that release.
Between releases the manifest on `main` still names the last release, so hooks and skills from
`main` run against the previous release's binary.

## Decision

1. **Trunk-based.** Work happens on short-lived branches, each in its own worktree, and reaches
   `main` by fast-forward. There is no long-lived feature or release branch.
2. **`main` stays green.** After every push to `main` the CI run on `main` must pass, including the
   Windows leg. A red `main` is fixed or reverted before other work lands.
3. **Breaking changes are batched into three releases:**

   | Release | Theme | Contents |
   |---|---|---|
   | 0.34 | Surface | verb tools (ADR-0018, ADR-0020, ADR-0022), agent comms v2 (ADR-0021), daemon generations (ADR-0024) |
   | 0.35 | Formats | the worktree index, the retrieval rewrite, store versioning (ADR-0025); one index wipe for all of them |
   | 0.36 | Libraries | the crate split and the single engine API (ADR-0023) |

   A breaking change that belongs to a theme waits for its release. Patch releases stay compatible
   in tools, protocol and formats.
4. **Plugin skew is a known hazard, with two rules.** Between a surface-breaking change landing on
   `main` and the next release, the hooks and skills that hosts fetch from `main` can name tools
   the pinned binary does not have.
   - A release is cut promptly after a surface-breaking change lands. Such changes are not left on
     `main` to wait for unrelated work.
   - Before each release a skew check runs the plugin's launcher against the built binary: it
     requests `tools/list` and makes one call to every tool the plugin's hooks, skills and commands
     name. A tool that is named but missing, or a call that fails on its arguments, blocks the
     release.
5. **Version surfaces are bumped only by `mise run release:sync-version`.** No surface is edited by
   hand.

## Consequences

- ADR-0010 is superseded. Its lockstep-versioning clause continues as rule 5; everything else in it
  is void.
- Users take three breaking upgrades in a row, and one full rescan at 0.35. Batching keeps it to
  one rescan; the price is that finished format work waits for 0.35.
- Rule 4 narrows the skew window but does not close it. For as long as a surface-breaking change
  is on `main` without a release, a host that updates the plugin gets hooks that suggest tools its
  binary rejects. The skew check runs at release time and so does not see that window.
- A second, shorter window exists at release time: after the version bump lands and before the
  release assets finish uploading, the launcher falls back to the newest published release
  (`scripts/mcp-launch.sh`), which is again older than the plugin content.
- "Promptly" is not a number. The rule is enforceable only as a review norm: a surface-breaking
  merge should come with the release already prepared.
- Trunk-based work with fast-forward only means a branch must be rebased onto `main` before it
  lands, and a breaking change cannot be parked on a branch for long without drifting.
- The 0.34 contents come from several branches (`feat/comms-v2-api`, `fix/daemon-coexistence`).
  They must all land before 0.34 is cut, or the surface breaks twice.

## Alternatives considered

- **A long-lived release branch**, with `main` free to break and releases cut from the branch —
  rejected: hosts fetch plugin content from the default branch, so `main` is what users run
  whatever branch the binary is built from. A release branch would move the skew, not remove it,
  and it brings back the divergence and cherry-picking that ADR-0010 was written to manage.
- **Serve plugin content from a release tag instead of the default branch** — this would close the
  skew window. The marketplace entry's `source` would have to name this repository at a tag in
  place of the relative path `./plugin`, `release:sync-version` would have to rewrite that tag in
  lockstep with the other surfaces, and the tag would have to exist before the commit naming it is
  fetched. Not adopted now: the same plugin content is mirrored for other harnesses (the Codex,
  Cursor and Gemini manifests), each with its own way of fetching, so pinning one marketplace
  leaves the rest on the default branch. It is the first thing to revisit if the skew check does
  not contain the problem.
- **Land each breaking change in its own release** — rejected: 0.35 alone would be three index
  wipes, and each surface change would need its own round of plugin, skill and doc updates.
- **One release for everything** — rejected: the crate split depends on the final formats
  (ADR-0023), and a single release would hold the surface fixes back until the split is done.
