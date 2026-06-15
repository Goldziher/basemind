# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0]

First feature-complete release. Adds the kreuzberg document tier surface
(reranker, keywords, NER, summarization, language detection, TOON output) on
top of the initial code-map server, with schema-driven config across TOML /
CLI / MCP / env vars.

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

[Unreleased]: https://github.com/Goldziher/basemind/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Goldziher/basemind/releases/tag/v0.1.0
