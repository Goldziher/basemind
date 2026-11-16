---
status: accepted
date: 2026-11-16
supersedes: "0026"
amends: ["0021", "0022", "0023"]
---

# ADR-0027: Release and branch policy — trunk-based, one breaking release

- **Deciders:** the repository owner, on 2026-11-16
- **Related:** supersedes ADR-0026; amends ADR-0021, ADR-0022 and ADR-0023 (the release each
  names); ADR-0015, ADR-0016, ADR-0018, ADR-0020, ADR-0024, ADR-0025

## Context

ADR-0026 spread the pending breaking changes over three minor releases: 0.34 (the verb-tool
surface, the audit fixes, the daemon's protocol generations), 0.35 (agent comms v2 and the format
work) and 0.36 (the library crates). It rejected a single release for two reasons: the crate split
depends on the final formats, and one release would hold the surface fixes back until the split is
done.

Both reasons are weaker now. The work is being done as one continuous run of branches onto `main`,
so the three releases would follow each other closely, and each would cost users a full rescan
(every minor rescans, ADR-0025) and a wire-protocol step. The last thing that kept the library
crates from publishing was `xberg`, a git dependency pinned by `rev` in `Cargo.toml`; crates.io
does not accept a crate with a git dependency (ADR-0023, rule 4).

`main` at `b028c982` still carries version 0.33.0, comms `PROTO_VER` 5 (`src/comms/protocol.rs`)
and `RELAY_PROTO_VER` 1 (`src/comms/relay.rs`).

## Decision

1. **One breaking release, 0.34.0, carries all of it:** the verb-tool surface (ADR-0018, ADR-0020,
   ADR-0022), the daemon's protocol generations (ADR-0024), the storage and durability fixes, agent
   comms v2 (ADR-0021), the worktree index (ADR-0016), the retrieval pipeline, state versioning
   (ADR-0025), the crate split into core, retrieval and engine with one engine API (ADR-0023), and
   cross-harness agent launch. There is no 0.35 or 0.36 in this plan. Where ADR-0021 and ADR-0023
   name release 0.35 or 0.36, read 0.34.0.
2. **`xberg` is consumed from crates.io,** not from a git pin. That removes the last publishing
   blocker of ADR-0023 rule 4: retrieval, engine and the root crate publish with 0.34.0.
3. **The order inside the release is ADR-0023's order,** now as the order of landing on `main`:
   layering in place, then the format work, then extraction into crates.

Retained from ADR-0026, restated here because that record is superseded as a whole:

1. **Trunk-based.** Work happens on short-lived branches, each in its own worktree. Maintainers
   fast-forward a branch onto `main` and push; outside contributions come as pull requests and are
   squash-merged. There is no long-lived feature or release branch.
2. **`main` stays green.** CI runs on `main` after every push, including the Windows leg. A red run
   is fixed or reverted before anything else lands.
3. **Breaking changes go in a minor release; every minor release rescans.** Patch releases stay
   compatible in tools, protocol and formats. A tool that ADR-0022's gate removes goes in the next
   minor release, not in a patch.
4. **Plugin content lands with the binary change it names.** Plugin content (hooks, skills,
   commands or manifests under the plugin trees) that names a tool, mode or command reaches `main`
   in the same branch as, or after, the code that serves it, so content and code agree at every
   commit of `main`. The manifests keep pinning the last release until 0.34.0 is cut; the skew
   check of rule 5 runs against the binary built from the same commit, not the pinned one.
5. **Release gates.** 0.34.0 is cut only when all of these pass: the plugin skew check (ADR-0026
   rule 6: every name in `TOOL_NAMES` of `plugin/hooks/session-start` is advertised by the binary
   started through the plugin launcher, and each tool's example call succeeds), the
   upgrade-from-release test (`tests/upgrade_from_release.rs`), CI on Windows, and the harden
   harness (`tests/harden.rs`).
6. **The release needs the owner's go-ahead.** Green gates allow a release; they do not start one.
7. **Version surfaces are bumped only by `mise run release:sync-version`.**

## Consequences

- Users rescan once instead of three times, and take one breaking upgrade.
- Each wire protocol jumps once: comms `PROTO_VER` goes from 5 to 7 and `RELAY_PROTO_VER` from 1 to
  2 in the same release, with no release at the intermediate values. Until every session runs
  0.34.0, an older client is refused by a newer daemon (ADR-0021).
- The live plugin skew lasts longer than ADR-0026 intended. Plugin content on `main` already names
  the verb tools while the manifests pin 0.33.0, a binary without them. ADR-0026 cut 0.34 early to
  end that; now it ends only when the whole of 0.34.0 ships, and it grows as comms v2 content
  lands. An installed plugin tracking `main` is mismatched for that whole period.
- A larger release is harder to bisect and to blame for a regression. What limits that: every
  branch lands on `main` on its own with CI green, so `main` bisects by branch, and the gates of
  rule 5 run on the result.
- ADR-0026 is superseded. ADR-0010's lockstep-versioning clause continues as rule 7.
- Follow-on work that ADR-0026 listed and that is still open: the retrieval pipeline has no ADR in
  this log, and neither does cross-harness agent launch; ADR-0015 is still proposed and assigned to
  no release. The skew check was not on `main` when ADR-0026 was written; whether it is now is not
  verified here, and it must exist before the release.
- ADR-0022 sets "release 0.35" as the point from which a tool without a task kind counts as
  failing its value gate. Read 0.34.0 there too: the gate is evaluated before this release.

## Alternatives considered

- **Keep the three releases of ADR-0026** — rejected: three rescans and three protocol steps for
  users within a short time, and three rounds of release work (plugin content, docs, upgrade
  testing), for changes that are landing on `main` back to back anyway. Its one advantage, ending
  the plugin skew early, is given up knowingly.
- **Two releases: 0.34 with the surface, daemon and comms work, 0.35 with the formats and the crate
  split** — rejected: it still costs a second rescan and a second breaking upgrade, and it draws
  the line through work that shares code. Comms v2 and state versioning both change the stores, so
  the first release would ship formats the second replaces.
- **Keep the `xberg` git pin and publish only core** — rejected: embedders wanting documents or
  vector search would have to take retrieval and engine as git dependencies and could not publish
  themselves (ADR-0023, consequences).
