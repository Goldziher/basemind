---
status: superseded
date: 2026-11-15
supersedes: "0010"
superseded-by: "0027"
---

# ADR-0026: Release and branch policy — trunk-based, three breaking releases

- **Deciders:** basemind maintainers
- **Related:** supersedes ADR-0010; ADR-0015, ADR-0016, ADR-0018, ADR-0021, ADR-0022, ADR-0023,
  ADR-0024, ADR-0025

## Context

ADR-0010 is the standing branch and release policy. Most of it describes work that no longer
exists: it routes UI work to a long-lived `feat/agent-layer` branch, and its cherry-pick mechanism,
UI feature gate, desktop app and docker image all concern that branch. The UI is gone (ADR-0006 is
withdrawn; ADR-0017 superseded ADR-0007 and the graph roadmap). One clause is still in force:
versions are bumped in lockstep through the single sanctioned workflow.

Since then five accepted ADRs each carry a breaking change: the verb-tool surface (ADR-0018), agent
comms v2 (ADR-0021), protocol generations (ADR-0024), store versioning (ADR-0025) and the crate
split (ADR-0023). Nothing says in which release each lands.

**Plugin skew is live.** The marketplace entry in `.claude-plugin/marketplace.json` has
`"source": "./plugin"`, a path inside this repository, so a host that installs or updates the
plugin takes its hooks, skills and commands from whatever commit of the default branch it fetched.
The binary is pinned separately: the launcher (`scripts/mcp-launch.sh`) reads the version from the
plugin manifest and downloads that release. Today `main` is 60 commits past `v0.33.0`;
`plugin/hooks/session-start` on `main` lists the fifteen verb tools; and the manifests pin 0.33.0,
a binary that has none of them. Every host that updated the plugin since is told to call tools its
binary does not advertise.

## Decision

1. **Trunk-based.** Work happens on short-lived branches, each in its own worktree. Maintainers
   fast-forward a branch onto `main` and push; outside contributions come as pull requests and are
   squash-merged. There is no long-lived feature or release branch.
2. **`main` stays green.** CI runs on `main` after every push, including the Windows leg. A red run
   is fixed or reverted before anything else lands.
3. **Breaking changes are batched into three releases:**

   | Release | Contents |
   |---|---|
   | 0.34 | the verb-tool surface as it is on `main` (ADR-0018, ADR-0020, ADR-0022), the verb-tool and storage audit fixes, the daemon's protocol generations (ADR-0024) |
   | 0.35 | agent comms v2 (ADR-0021: three agent tools replace `agents`; comms protocol 5 to 6, relay 1 to 2); in-place layering first (ADR-0023); then the format work: the worktree index (ADR-0016), the retrieval rewrite, store versioning (ADR-0025) |
   | 0.36 | library crates and one engine API (ADR-0023) |

   0.34 is cut as soon as its contents are on `main` and CI is green, because it ends the live
   skew. A breaking change waits for its release. Patch releases stay compatible in tools, protocol
   and formats. A tool that ADR-0022's gate removes goes in the next of these releases, not in a
   patch.
4. **Every minor release triggers an automatic rescan,** because derived stores are tied to the
   release minor (ADR-0025). 0.35 is the release whose formats are redesigned; 0.34 and 0.36
   rescan all the same.
5. **Plugin content waits for its binary.** A change to plugin content (hooks, skills, commands or
   manifests under the plugin trees) that names a tool, mode or command the currently pinned binary
   lacks may reach `main` only when the release carrying that binary change is cut within one
   working day. Otherwise it waits on its branch. That is the one stated exception to short-lived
   branches.
6. **A skew check gates every release.** It builds the binary, starts it through the plugin
   launcher, calls `tools/list`, and asserts that every tool name in the plugin's machine-readable
   list (`TOOL_NAMES` in `plugin/hooks/session-start`) is advertised. It then makes one call per
   advertised tool with the example arguments from that tool's description; a call that fails on
   its arguments blocks the release. This check does not exist yet and is to be written before
   0.34.
7. **Version surfaces are bumped only by `mise run release:sync-version`.** No surface is edited by
   hand.

## Consequences

- ADR-0010 is superseded. Its lockstep-versioning clause continues as rule 7; the rest is void.
- Users take three breaking upgrades in a row and a full rescan at each.
- The repository's workflow rule reads "Squash merge PRs" and "Never force-merge without CI
  passing". It holds for pull requests; rule 1 adds the maintainers' fast-forward path, where CI
  runs after the push, and the rule text must be updated to say so.
- Rule 5 can hold finished plugin work on a branch for weeks, and that branch must be rebased onto
  `main` before it lands. It trades branch age for hosts whose hooks match their binary.
- Rule 6 reads one list. Tool names in the prose of skills and commands are not checked; only
  `tools/list` against `TOOL_NAMES`, and each tool's own example, are. Each tool description must
  therefore carry an example call, which none is required to today.
- A shorter skew window remains at release time: after the version bump lands and before the
  release assets finish uploading, the launcher falls back to the newest published release
  (`scripts/mcp-launch.sh`), which is older than the plugin content.
- "The retrieval rewrite" in 0.35 has no ADR yet and needs one before the work starts. ADR-0015
  (bounded document extraction) is still proposed and is assigned to no release.
- The 0.34 contents come from several branches (`fix/verb-tool-audit`, `fix/storage-audit`,
  `fix/daemon-coexistence`); 0.34 waits for all three.
- 0.35 changes the comms protocol inside a protocol generation, so until every session runs 0.35
  an older client is refused by a newer daemon (ADR-0021).

## Alternatives considered

- **Agent comms v2 in 0.34 with the rest of the surface** — rejected: 0.34 would wait for it, and
  the live skew would last as long.
- **A long-lived release branch**, with `main` free to break and releases cut from the branch —
  rejected: hosts fetch plugin content from the default branch, so `main` is what users run
  whatever branch the binary is built from.
- **Serve plugin content from a release tag instead of the default branch** — this would close the
  skew window. The marketplace `source` would have to name this repository at a tag,
  `release:sync-version` would have to rewrite it in lockstep, and the tag would have to exist
  before the commit naming it is fetched. Not adopted now: the same content is mirrored for other
  harnesses (the Codex, Cursor and Gemini manifests), each with its own way of fetching. It is the
  first thing to revisit if rules 5 and 6 do not contain the problem.
- **Decouple derived-store versions from the release minor**, so that only 0.35 rescans — not
  decided here; it would change ADR-0025 and the bump script (`scripts/release-bump.sh`).
- **Land each breaking change in its own release** — rejected: each surface change would need its
  own round of plugin, skill and doc updates, and each format change its own migration testing.
- **One release for everything** — rejected: the crate split depends on the final formats
  (ADR-0023), and a single release would hold the surface fixes back until the split is done.
