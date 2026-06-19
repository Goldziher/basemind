# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

<!-- Keep a Changelog repeats Added/Changed/Fixed headings per version. -->
<!-- markdownlint-disable MD024 -->

## [Unreleased]

## [0.4.0] — 2026-06-19

Minor release: `RELEASE_MINOR` bumps 3 → 4, so the blob + index schema versions advance.
The first `basemind scan` / `serve` after upgrading rebuilds the cache in place. Prebuilt binaries
now ship `--features full` (96 document formats, OCR, embeddings, reranker, semantic search, web
crawl, shared memory). **Windows asset triple changed:** `x86_64-pc-windows-gnu` → `x86_64-pc-windows-msvc`
(ONNX Runtime ships no MinGW prebuilts); anyone hard-coding the old asset name must update.

### Added

- **Full-feature prebuilt binaries via native per-platform runner.** All prebuilt binaries
  (Homebrew, npm, pip, GitHub Releases) now ship `--features full`: 96 document formats (PDF,
  Office, email, HTML, archives, structured, source, markup), OCR (Tesseract), HEIC/AVIF
  (libheif), embeddings + reranker + NER, shared memory, and web crawl. First use downloads ML
  models over the network; binaries are larger. Replaced goreleaser with a new native
  per-platform CI runner pipeline.
- **Enforced binary checksum verification.** All install paths (npm, pip/uvx, direct download via
  `mcp-launch.sh`) now verify the binary's sha256 against `basemind_<version>_checksums.txt` and
  fail closed on mismatch or missing checksum. (This was previously claimed in the README but not
  implemented.)

### Changed

- **`find_references` and `find_implementations` now do real substring matching.** Previously
  documented as substring matching but actually exact-only. Now genuinely substring (case-sensitive),
  so `Foo::bar()` and `bar()` both match `name="bar"`.

### Fixed

- **SSRF denylist + redirect revalidation for web crawler.** Added safeguards to prevent server-side
  request forgery on the `web_scrape` and `web_crawl` tools.
- **Configured embedder preset.** Eliminates cross-process vector state wipe, improving performance
  and cache isolation.
- **Atomic memory writes.** Prevents torn writes on concurrent `memory_put` across sessions.
- **Index panic on corrupted entries.** Fixed a panic when the Fjall index contained malformed keys
  (now degrades gracefully).
- **Git blame robustness.** Improved handling of edge cases (missing commits, detached HEAD, shallow
  repos).
- **Config layer precedence fixes.** MCP > CLI > env > TOML > defaults now strictly enforced.
- **Hot-path allocation cuts.** Reduced allocations in scanner, extract, and query paths; removed
  unnecessary `String::from_utf8` round-trips and clones in the Fjall secondary-index scans.

## [0.3.0] — 2026-06-18

Minor release: `RELEASE_MINOR` bumps 2 → 3, so the blob + index schema versions advance.
Unlike previous minor bumps, the cache is **no longer hard-wiped** — see "Durable cache
refresh" below. The first `basemind scan` / `serve` after upgrading re-extracts in place.

### Added

- **Full MCP↔CLI parity.** Every MCP tool now has a `basemind` CLI subcommand, running the
  identical in-process tool code against the same on-disk index (read-only, no lock — safe
  to run alongside a live `basemind serve`). Human-readable output by default, `--json` for
  the raw structured response. New groups: `basemind query` (outline, symbol, search,
  references, callers, implementations, call-graph, grep, list-files, status, repo-info,
  dependents), `basemind git` (status, history, blame, diffs, churn, symbol-history),
  `basemind memory`, `basemind web`, and `basemind telemetry`.
- **Cache garbage collection + cleanup.** New `cache_gc` (reclaim orphaned blobs no view
  references), `cache_stats` (on-disk size + orphan accounting), and `cache_clear` MCP
  tools, plus `basemind cache gc|stats|clear` CLI commands. GC runs automatically in the
  background on `serve` startup. Blobs are shared across views; GC only sweeps blobs no
  view's index references, under the store lock so it never races a scan.
- **Continuous background watch.** `basemind serve` now watches the working tree and
  incrementally refreshes its index in the background by default (debounced), keeping
  queries fresh without manual `rescan`. Opt out with `basemind serve --no-watch` for very
  large repos / CI.
- **`basemind-cli` skill** documenting the CLI surface, and a restructured README with two
  equal install paths (MCP plugin vs CLI + skill).

### Changed

- **Durable cache refresh on schema bump.** A schema/version bump now refreshes the
  content-addressed blob store **in place** (re-extract overwrites blobs at their stable
  content-hash path; background GC reclaims the remainder) instead of deleting
  `.basemind/blobs/`. No window where the expensive extraction cache is gone. The Fjall
  secondary index (derived, cheap) still rebuilds from blobs on its own schema bump.
- **Git-cache schema** is now tied to `RELEASE_MINOR` so a release refreshes it consistently
  (it auto-rebuilds from git).

### Fixed

- **Tightened token-savings estimate.** The `telemetry_summary` "tokens saved" figure no
  longer scales with total corpus size (it previously credited ~5% of the whole repo per
  grep-style call). Baselines are now derived from the actual response payload, decoupled
  from corpus size.
- **Read-only store degrades gracefully on a schema bump.** A CLI query run before the first
  post-upgrade scan now reads an empty index ("run `basemind scan`") instead of erroring,
  and never opens the stale Fjall index.

## [0.2.6] — 2026-06-18

### Changed

- **Responsive two-line statusline with per-capability metrics.** The status line now
  renders a context row (model · output-style · vim · dir · branch · context%) above the
  basemind row, since a custom statusLine replaces Claude Code's default and cannot sit
  below it. The basemind row breaks telemetry down per capability — searches, git, docs,
  memory, web — showing only buckets with activity today, alongside calls and estimated
  tokens saved. Width comes from `$COLUMNS` with full/compact/minimal tiers; override with
  `BASEMIND_STATUSLINE` and hide the context row with `BASEMIND_STATUSLINE_CONTEXT=0`.

## [0.2.5] — 2026-06-18

### Added

- **PreToolUse guard hook drives basemind usage.** A configurable hook
  (`hooks/pre-tool-guard`) reaches the agent when it reaches for `Grep`/`Glob` and
  points it at the matching basemind tool — the only Claude Code lever that can also
  cover subagents. `BASEMIND_GUARD` selects the mode: `nudge` (default, advisory once
  per session), `redirect` (deny with a pointer to the basemind tool), or `off`.
- **Full-capability awareness in the agent surfaces.** The always-injected MCP
  `get_info` instructions and the `basemind` skill now describe the whole surface —
  code map across 300+ languages, full-text + semantic search, git intelligence,
  document RAG over 90+ file formats, shared memory, and web crawl — instead of a
  code-map-only framing. The README leads with the same capability set.

### Fixed

- **`Store::open` no longer races on a just-released lock.** `acquire_lock` retries the
  exclusive `flock` with a short backoff, fixing a macOS-only flake where a sequential
  open → close → open hit `Locked`.

## [0.2.4] — 2026-06-18

### Added

- **Context-economy operating discipline shipped across every agent surface.** The
  MCP `get_info` instructions, the `basemind` skill, the SessionStart hook, and the
  README now state one default workflow: these tools return paths, line numbers, and
  signatures rather than file bodies, so agents `outline` before reading,
  `search_symbols` / `find_references` / `workspace_grep` before grep, and `rescan`
  after edits. The SessionStart hook injects this every session instead of only
  nudging when the statusline is unset.

## [0.2.3] — 2026-06-18

### Added

- **`serve` auto-scans a fresh repo on startup.** When the working-view index is
  empty, the server kicks off an initial scan in the background (reusing the
  in-process `rescan` path, so it never contends for the Fjall lock). The agent
  no longer has to run `basemind scan` by hand — the statusline flips from
  "scanning…" to live counts on its own.
- **`.basemind/` is now self-ignoring.** The first time the store is created it
  writes `.basemind/.gitignore` containing `*`, so a user's repository never
  accidentally commits the machine-local index. An existing `.gitignore` is left
  untouched.

### Changed

- **README redesign.** Centered hero with a uniform badge row, a fenced
  statusline preview (replacing a broken image reference), and collapsible
  architecture / harness-setup sections. Clarified that registering the
  marketplace and installing the plugin are two distinct steps.

### Fixed

- **Nightly hardening CI is green again.** The `hardening` job built
  `--features full` (kreuzberg's libheif + tesseract stack) but only installed
  protoc, so the `libheif-sys` build failed. The job now installs the codec dev
  libraries and builds libheif 1.23.0 from source (cached across runs).

## [0.2.2] — 2026-06-17

### Added

- **Claude Code plugin now auto-installs the basemind binary.** The MCP launcher
  (`scripts/mcp-launch.sh`) tries version-matched cached binaries, then npx
  (npm package), uvx (PyPI package), and finally downloads the prebuilt binary
  from the GitHub release with checksum verification. Override the launch strategy
  with `BASEMIND_LAUNCHER=auto|npx|uvx|download`.
- **SessionStart hook pre-warms the binary and nudges statusline setup.** The
  hook runs in the background on session start to ensure the first tool call isn't
  a cold install, and suggests `/bm-statusline` if the statusline isn't yet
  configured.
- **`/bm-statusline` slash command wires the statusline into
  `~/.claude/settings.json`.** Plugins cannot set the main statusline
  automatically; `/bm-statusline` is the one-step opt-in.

## [0.2.1] — 2026-06-17

### Fixed

- **Claude Code slash commands now load.** The `/bm` and `/bm-stats` commands
  were under `.claude-plugin/commands/`, which Claude Code ignores — plugin
  component directories must sit at the plugin root, and basemind's marketplace
  `source` is `"./"`, so the plugin root is the repo root. Moved `commands/`
  (and confirmed `skills/`) to the repo root; `.claude-plugin/` now holds only
  the manifests + statusline. The Codex / Cursor / OpenCode trees keep their own
  in-tree copies (their plugin root is their own directory), kept in sync by
  `scripts/sync-plugin-skills.sh`.

### Changed

- **Publish workflow no longer hard-fails on a missing Homebrew token.** The
  GitHub-release-assets job skips the Homebrew tap publisher when
  `HOMEBREW_TOKEN` is absent (and runs it when present), so an optional tap
  update can't fail the binary release.

## [0.2.0] — 2026-06-17

**Minor release — schema wipe (`RELEASE_MINOR=2`).** The blob and inverted-index
formats are versioned to the release minor, so the next `basemind scan` rebuilds
`.basemind/` from source. This is the intentional migration path; no user action
beyond a rescan is needed.

### Changed

- **Dependencies on crates.io.** `kreuzberg` moved from the `v5.0.0-rc.15` git pin
  to the published crates.io release `=5.0.0-rc.18`; the temporary `allow-git`
  exemptions in `deny.toml` are dropped. With no remaining git dependencies,
  `cargo publish` to crates.io is unblocked — basemind now ships on all four
  registries (crates.io, npm, PyPI, Homebrew) from a single tag.
- **README is now a positioning document.** Rewritten from a code-map feature
  inventory into a full AI-context-layer overview: four pillars (Code, Documents,
  Memory, Web), a feature table mapping every MCP tool to its backend, a
  three-flavour quickstart, comparisons against grep / vector-only RAG /
  repo-map indexers / GitHub search, and a differentiators section. Sub-package
  READMEs (npm / PyPI / OpenCode) collapse to install stubs linking the root.
- **Aligned package metadata across all surfaces.** One canonical description and
  a positioning-led 5-keyword set (`mcp`, `agent-context`, `rag`, `code-map`,
  `tree-sitter`) now ship identically across `Cargo.toml`, the npm / PyPI
  manifests, and every harness plugin manifest. GitHub repo description + topics
  updated to match.

### Added

- **Redesigned Claude Code statusline.** Bright 256-color palette (no more
  low-contrast dim text), a `◆` brand mark, a dedicated serve/scan freshness
  dot, and width-aware wide/narrow layouts. The file count now reads the L1 blob
  set directly (exact, deduped) instead of estimating from index bytes; scan
  recency reads the view index mtime; an empty repo renders an actionable
  `run: basemind scan` hint instead of a blank line. The intelligence row
  (documents / memory / web) appears when a LanceDB store is present.
- **Agent-facing skills + slash commands bundled into every plugin tree.** The
  `basemind` routing skill and `basemind-stats` dashboard, plus `/bm` and
  `/bm-stats` slash commands, now ship inside `.claude-plugin/`, `.codex-plugin/`,
  `.cursor-plugin/`, and the `basemind-opencode` npm tarball — so an installed
  plugin teaches the agent how to use the tools without manual onboarding. A new
  `scripts/sync-plugin-skills.sh` (wired as a prek hook) keeps the canonical
  source and the per-harness copies in lock-step.

## [0.1.1] — 2026-06-15

### Fixed

- **`npm-package`** — `npx basemind …` (and equivalent `npm install -g basemind`
  invocations) used to silently fall through to whatever `basemind` already lived
  on `$PATH` because npm skipped creating the `node_modules/.bin/basemind`
  symlink when the `bin:` target was missing at install time (the native binary
  is downloaded by `postinstall`). The wrapper now ships a Node.js shim at
  `bin/basemind.js` that `spawnSync`'s the downloaded native binary, so the
  symlink target exists at install time and npx routes correctly through the
  wrapper.
- **`publish.yaml`** — re-enabled the Homebrew tap step now that
  `HOMEBREW_TOKEN` is on the path to being provisioned. Set the secret on the
  basemind repo before pushing the next tag; goreleaser will then auto-update
  `Goldziher/homebrew-tap`'s `Formula/basemind.rb`.

## [0.1.0] — 2026-06-15

**Initial public release.** First minor wipes the schema (`RELEASE_MINOR=1`)
so any pre-tag `.basemind/` cache rebuilds on next scan — intentional.

Feature-complete code-map server with the kreuzberg document tier surface
(reranker, keywords, NER, summarization, language detection, TOON output) and
schema-driven config across TOML / CLI / MCP / env vars, distributed across
all major coding-agent harnesses (Claude Code, Codex, Cursor, Gemini, Factory
Droid, OpenCode, Copilot CLI, and the generic MCP path) plus npm / PyPI /
crates.io.

### Added

- **`basemind scan`** — rayon-parallel scanner that indexes a workspace into
  content-addressed msgpack blobs (`.basemind/blobs/`) plus a Fjall-backed
  inverted index (`.basemind/views/<view>/index.fjall/`). Two extraction tiers
  ship: L1 outlines (symbols, signatures, imports, docs) and L2 call sites;
  L3 structural hash available for symbol-history diffing.
- **`basemind serve`** — stdio MCP server (`rmcp`) exposing the full code-map
  and git-history tool surface (`outline`, `search_symbols`, `find_references`,
  `find_callers`, `list_files`, `dependents`, `repo_info`, `status`,
  `symbol_history`, `working_tree_status`, `recent_changes`,
  `commits_touching`, `find_commits_by_path`, `diff_file`, `diff_outline`,
  `hot_files`, `blame_file`, `blame_symbol`).
- **Dynamic 300+ language coverage** via
  [tree-sitter-language-pack](https://github.com/kreuzberg-dev/tree-sitter-language-pack).
  Hand-written `.scm` overrides ship for Rust, Python, TypeScript, TSX,
  JavaScript, Go. Other languages for which TSLP ships a vendored `tags.scm`
  (kotlin, csharp, swift, cpp, scala, solidity, lua, …) get best-effort
  symbol + call extraction via the fallback adapter that rewrites
  GitHub-standard `@definition.*` / `@reference.call` captures into
  basemind's `@symbol.*` / `@call.*` shape.
- **Schema-driven config across TOML / CLI / MCP / env vars.** Rust types in
  `src/config/` derive `schemars::JsonSchema`; the snapshot at
  `schema/basemind-config-v1.schema.json` is regenerated from those types and
  asserted by `tests/config_schema.rs`. Adding a config field lights up all
  four surfaces via `#[command(flatten)]` (clap) and `#[serde(flatten)]` (MCP
  params). Precedence: MCP > CLI > env > TOML > defaults, with per-field
  provenance tracking via `src/config/source.rs`.
- **Document tier** — `search_documents` MCP tool over Lance-backed embedding
  index with kreuzberg ingestion. Per-query overrides on every `documents.*`
  and `llm.*` setting, plus `entity_category` and `keywords_contains`
  post-filters.
- **TOON wire format** for MCP responses via
  `documents.output.format = "toon"` (or `--documents-output-format toon`).
  Round-trip parity with JSON asserted in `tests/mcp_smoke.rs`.
- **Language-aware ingestion.** `documents.language.{auto_detect,
  min_confidence, detect_multiple}` flows into kreuzberg's
  `LanguageDetectionConfig` and the chunking tokenizer. ISO 639-3 codes (e.g.
  `"fra"`) surface on `FileMapDoc.detected_languages`.
- **Cross-encoder reranker** as a post-step on `search_documents`. Off by
  default; `documents.reranker.{enabled,preset,top_k}` opts in. Preset is
  validated upfront against `kreuzberg::get_reranker_preset`; reranked index
  is bounds-checked before reorder.
- **Keyword extraction (YAKE / RAKE) and named entity recognition** at
  extract time. New tail fields on `FileMapDoc` (`keywords`, `entities`)
  with `#[serde(default)]` — blob-compatible with prior blob shapes
  (asserted by `tests/schema_bump.rs`).
- **Extractive + abstractive summarization** via `documents.summarization`.
  Abstractive routes through liter-llm with the new top-level `[llm]`
  section (model in `provider/model` form, api_key, base_url, temperature,
  timeout, retries, max_tokens). NER backend `llm` now wires the resolved
  `LlmConfig`.
- **`SecretString` newtype + `ApiKey` enum** (`Literal | Env | Unset`).
  Secrets mask to `"<redacted>"` in `Debug` / `Display` and across
  `Serialize` (including the toml→serde_json validation round-trip).
- **Real-OSS hardening harness** (`tests/harden.rs`, `./scripts/harden.sh`)
  — clones 8 upstream repos (ripgrep, tokio, typescript, react, django,
  requests, gin, ripgrep-shallow), exercises every MCP tool against each,
  and pins canary lower bounds (tokio: `find_references("spawn") >= 200`,
  django: `find_references("get") >= 200`,
  react: `search_symbols("useState") >= 20`,
  ripgrep-shallow truncation surfaces).
- **Schema sync to release version** — `RELEASE_MINOR` in `src/version.rs`
  drives both `INDEX_SCHEMA_VER` and the blob `SCHEMA_VER`. Minor-release
  bumps wipe `.basemind/` on next scan; patch releases stay compatible.
- Distribution: `brew install Goldziher/tap/basemind`,
  `npm install -g basemind`, `pip install basemind`,
  `cargo install basemind --locked`. Precompiled binaries on GitHub
  Releases for `{x86_64,aarch64}-{linux-gnu,apple-darwin}` and
  `x86_64-pc-windows-gnu`.
- **Per-harness install matrix** — manifests + skill bundle for every major
  coding-agent harness, all bumped in lock-step by
  `task release:sync-version VERSION=…`:
  - **Claude Code** — `.claude-plugin/plugin.json` + `marketplace.json` +
    `statusline.sh` (live one-line summary of the indexed map with true-color
    brand mark, freshness dot, and call/token-savings telemetry from
    `.basemind/telemetry.jsonl`). Install via
    `/plugin marketplace add Goldziher/basemind` then
    `/plugin install basemind@basemind`.
  - **Codex CLI / App** — `.codex-plugin/plugin.json` with full `interface`
    block (`displayName`, category `Developer Tools`, `capabilities`,
    `defaultPrompt`, `brandColor`). `scripts/sync-to-codex-plugin.sh` mirrors
    into a fork of the `openai/plugins` marketplace.
  - **Gemini CLI** — root-level `gemini-extension.json` with `contextFileName`
    and `mcpServers`. Install via
    `gemini extensions install https://github.com/Goldziher/basemind`.
  - **Cursor** — `.cursor-plugin/plugin.json`.
  - **OpenCode** — published as the
    [`basemind-opencode`](https://www.npmjs.com/package/basemind-opencode)
    npm package. Install via
    `{ "plugin": ["basemind-opencode@latest"] }` in `opencode.json`. Skills
    are bundled into the tarball; the plugin shim does dual-mode resolution
    (bundled-path-first, repo-root-fallback) so monorepo dev and npm install
    both work without duplication.
  - **Factory Droid / GitHub Copilot CLI** — reuse the existing
    `.claude-plugin/marketplace.json` per their published patterns.
  - Every manifest carries the same canonical user-facing description and
    crates.io-capped 5-keyword set (`mcp`, `tree-sitter`, `code-map`,
    `scanner`, `indexer`).
- **Release pipeline** — `.github/workflows/publish.yaml` publishes on
  every `v*` tag: GitHub release assets (cross-compiled binaries via
  goreleaser + zig), crates.io, npm × 2 (`basemind` binary wrapper +
  `basemind-opencode` OpenCode plugin), and PyPI. Per-registry idempotent
  via existing-version detection; OIDC-driven trusted publishers on all
  four registries.

### Changed

- `tree-sitter-language-pack`: `=1.9.0-rc.27` → `=1.9.0-rc.45`. Hierarchical
  `data_extraction` + 17 data formats; rc.40–45 are CI/codegen-only.
- `kreuzberg` bumped to the reranker / LLM API surface
  (`=5.0.0-rc.7` baseline → published rc covering reranker + LLM at publish
  time).
- `alloc-stdlib = "=0.2.2"` pin lifted (no longer binding after lock
  re-resolve; single `alloc-no-stdlib 2.0.4` in the tree).
- `src/mcp/types.rs` + `src/mcp/helpers.rs` split into `_documents.rs`
  siblings to stay under the 1000-line cap.

### Performance

- Harden 8/8 green across ripgrep / tokio / typescript / react / django /
  requests / gin / ripgrep-shallow. All canaries pass. Per-repo scan times
  within baseline: typescript 21.7 s (81 324 files), tokio 0.2 s (859
  files), django 2.5 s (7 061 files), react 2.2 s, requests 0.7 s, gin
  1.0 s, ripgrep 4.0 s, ripgrep-shallow 0.16 s. All 25 MCP tools clean
  across all repos.
- `search_documents` post-processing releases the store read-lock before
  blob I/O; `ahash::AHashMap` / `AHashSet` on the post-filter path.

[Unreleased]: https://github.com/Goldziher/basemind/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/Goldziher/basemind/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/Goldziher/basemind/compare/v0.2.6...v0.3.0
[0.2.6]: https://github.com/Goldziher/basemind/compare/v0.2.5...v0.2.6
[0.2.5]: https://github.com/Goldziher/basemind/compare/v0.2.4...v0.2.5
[0.2.4]: https://github.com/Goldziher/basemind/compare/v0.2.3...v0.2.4
[0.2.3]: https://github.com/Goldziher/basemind/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/Goldziher/basemind/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/Goldziher/basemind/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/Goldziher/basemind/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/Goldziher/basemind/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/Goldziher/basemind/releases/tag/v0.1.0
