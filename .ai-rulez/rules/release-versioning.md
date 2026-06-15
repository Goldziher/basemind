---
priority: high
---

# Release Versioning

Basemind versions are bumped across every shipped surface in lock-step. The single sanctioned bumper is
`task release:sync-version VERSION=X.Y.Z` (script: `scripts/release-bump.sh`, also bumps every plugin
manifest version in lock-step). Never hand-edit one
surface without the others — `cargo publish` / npm / PyPI all enforce version uniqueness, so
the workflow's per-registry skip detection breaks on a partial bump.

## Surfaces (all updated by `release:sync-version`)

| Surface | Format | Notes |
|---|---|---|
| `Cargo.toml` `[package] version` | `X.Y.Z` or `X.Y.Z-rc.N` | Source of truth. |
| `npm-package/package.json` `"version"` | `X.Y.Z` or `X.Y.Z-rc.N` | Same shape as Cargo. |
| `pip-package/pyproject.toml` `version` | `X.Y.Z` or `X.Y.ZrcN` | PyPI canonical form. |
| `pip-package/basemind/__init__.py` `__version__` | matches `pyproject.toml` | Used by `downloader.py` to compute the GH release URL. |
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

The `meta` job detects already-published versions on crates.io / npm / PyPI / GH release
assets and skips downstream jobs accordingly — making the workflow idempotent across re-runs
of the same tag.
