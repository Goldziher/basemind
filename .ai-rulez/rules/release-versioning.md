---
type: Decision
title: Release Versioning
x-ai-rulez:
  kind: rule
  id: release-versioning
  metadata:
    priority: high
---

# Release Versioning

Basemind versions are bumped across every shipped surface in lock-step. The single sanctioned bumper is
`mise run release:sync-version X.Y.Z` (script: `scripts/release-bump.sh`, also bumps every plugin
manifest version in lock-step). Never hand-edit one
surface without the others — `cargo publish` / npm / PyPI all enforce version uniqueness, so
the workflow's per-registry skip detection breaks on a partial bump.

## Surfaces (all updated by `release:sync-version`)

`scripts/check_release_versions.py <version>` is the authoritative list: the bump script and the
publish workflow both run it, and it fails on any surface that is missing or disagrees.

| Surface | Format | Notes |
|---|---|---|
| `Cargo.toml` `[package] version` | `X.Y.Z` or `X.Y.Z-rc.N` | Source of truth. |
| `crates/basemind-agents/Cargo.toml` `[package] version`, and the root's `basemind-agents` requirement | `X.Y.Z` or `X.Y.Z-rc.N` | Published to crates.io before `basemind` (ADR-0040). |
| `npm-package/package.json` `"version"` | `X.Y.Z` or `X.Y.Z-rc.N` | Same shape as Cargo. |
| `pip-package/pyproject.toml` `version` | `X.Y.Z` or `X.Y.ZrcN` | PyPI canonical form (`basemind`, binary wrapper). |
| `pip-package/basemind/__init__.py` `__version__` | matches `pyproject.toml` | Used by `downloader.py` to compute the GH release URL. |
| `plugin/.ai-rulez/config.plugins.toml` `[plugin] version` | `X.Y.Z` or `X.Y.Z-rc.N` | Source of every generated `plugin/` manifest and the Hermes package. |
| `plugin/.hermes/package/pyproject.toml` `version` | `X.Y.Z` or `X.Y.ZrcN` | Generated. PyPI canonical form (`basemind-hermes-plugin`, standalone Hermes plugin). |
| `plugin/.hermes/package/src/basemind_hermes_plugin/__init__.py` `__version__` | Cargo form | Generated plugin package version. |
| `plugin/.hermes/package/src/basemind_hermes_plugin/plugin.yaml` `version` | `X.Y.Z` or `X.Y.Z-rc.N` | Generated Hermes Agent plugin manifest (bundled in the plugin wheel). |
| `plugin/.hermes/plugins/basemind/{__init__.py,plugin.yaml}` | Cargo form | Generated copy of the Hermes plugin. |
| `plugin/{package.json,plugin.json,gemini-extension.json,kimi.plugin.json}` and `plugin/.{claude,codex,cursor,factory}-plugin/plugin.json` `version` | `X.Y.Z` or `X.Y.Z-rc.N` | Generated per-runtime manifests of the marketplace bundle. |
| `kimi.plugin.json` `"version"` | `X.Y.Z` or `X.Y.Z-rc.N` | Root Kimi manifest. |
| `src/version.rs` `RELEASE_MINOR` | `u16` | Bumped only when the MINOR component changes (or major-100 carry for `1.X` and beyond). |
| `package.json` `"version"` | `X.Y.Z` or `X.Y.Z-rc.N` | Workspace root marker (private). |
| `opencode-plugin/package.json` `"version"` | `X.Y.Z` or `X.Y.Z-rc.N` | `basemind-opencode` npm package. |
| `.claude-plugin/plugin.json` `"version"` | `X.Y.Z` or `X.Y.Z-rc.N` | Claude Code plugin manifest. |
| `.claude-plugin/marketplace.json` `plugins[0].version` | `X.Y.Z` or `X.Y.Z-rc.N` | Claude Code marketplace listing. |
| `.codex-plugin/plugin.json` `"version"` | `X.Y.Z` or `X.Y.Z-rc.N` | Codex plugin manifest. |
| `.cursor-plugin/plugin.json` `"version"` | `X.Y.Z` or `X.Y.Z-rc.N` | Cursor plugin manifest. |
| `gemini-extension.json` `"version"` | `X.Y.Z` or `X.Y.Z-rc.N` | Gemini CLI extension manifest. |

## Bump cadence

- **Patch** (`0.1.0` → `0.1.1`): blob + index format MUST stay compatible. `RELEASE_MINOR`
  unchanged. `release:sync-version` enforces this — it only edits `RELEASE_MINOR` if the minor
  component actually changed.
- **Minor** (`0.1.x` → `0.2.0`): `RELEASE_MINOR` bumps by 1. All users wipe `.basemind/` on
  next scan (intentional). Mention the wipe in the CHANGELOG `## [0.X.0]` heading.
- **Major** (`0.X.y` → `1.0.0`): `RELEASE_MINOR` jumps to `100`. After 1.0, the formula is
  `major * 100 + minor`. Encode this carefully — the `release:sync-version` script already does the
  arithmetic.
- **RC** (`X.Y.Z-rc.N`): the npm tag becomes `beta`, the PyPI form becomes `X.Y.ZrcN` (regex
  conversion in `release:sync-version` and in `publish.yaml`'s meta job). Cargo accepts the original
  form.

## Tag → workflow

Tags MUST be `v<version>` (e.g. `v0.1.0`, `v0.1.0-rc.1`). The publish workflow
(`.github/workflows/publish.yaml`) is gated on the `v[0-9]+.*` pattern and reads the version
back from the tag. `workflow_dispatch` accepts a manual tag input for re-runs.

## Release order (ADR-0045)

The marketplaces serve `./plugin` from `main`, so the release commit reaches `main` only after its
release is public. Nothing else lands on `main` while a release is in flight.

1. On a branch `release/vX.Y.Z` cut from `main`: `mise run release:sync-version X.Y.Z`, remove
   `.github/unreleased-surface.toml`, move the changelog's `[Unreleased]` entries under the version.
   Commit, push the branch, never `main`.
2. `gh workflow run ci.yaml --ref release/vX.Y.Z` and wait for it to go green, the harden harness
   included.
3. Tag that commit `vX.Y.Z` and push the tag.
4. `publish.yaml` publishes, then its `advance_main` job fast-forwards `main` to the tag and
   dispatches `ci` on `main`. If `main` moved, that job fails: merge the tag into `main` by hand.

A crate `publish_crates` pushes for the first time cannot use trusted publishing: crates.io creates
a crate only from a token-authenticated publish. Before tagging, publish its first version by hand
(`cargo publish -p <crate>` with an API token), then add `publish.yaml` as the crate's trusted
publisher on crates.io. `meta` refuses a release while any such crate is missing.

Nothing publishes until the `ci_gate` job has seen a successful dispatched or scheduled `ci` run on
the tagged commit, every test leg and the harden harness green. The `meta` job then checks every
version surface, refuses a Git dependency, a vendored crate that differs from its published version
or that crates.io does not have, and the unreleased-surface marker, and detects versions already on
crates.io / npm / PyPI / the GitHub release, so a re-run of the same tag publishes only what is
missing. Every archive is built and run on a clean runner
(`.github/workflows/release-build.yaml`) before it is uploaded.

## Unreleased plugin surface

The plugin manifests pin the last release, and the marketplaces serve `./plugin` from the default
branch, so plugin content on `main` runs against the pinned binary. When `main` names tools that
binary lacks, `.github/unreleased-surface.toml` must say so, with `pinned_version`, `since`,
`review_by` and `reason` (ADR-0035). `tests/plugin_tool_skew.rs` then reports every unserved name
instead of failing, fails once `review_by` has passed, and fails when the marker is stale. The
release commit removes the marker; `publish.yaml` refuses to release while it exists.
