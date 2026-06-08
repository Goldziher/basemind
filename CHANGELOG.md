# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-06-08

Initial public release.

### Added

- **`gitmind scan`** — rayon-parallel scanner that indexes a workspace into
  content-addressed msgpack blobs (`.gitmind/blobs/`) plus a Fjall-backed inverted index
  (`.gitmind/views/<view>/index.fjall/`). Two extraction tiers ship in this release:
  L1 outlines (symbols, signatures, imports, docs) and L2 call sites; L3 structural hash
  available for symbol-history diffing.
- **`gitmind serve`** — stdio MCP server (`rmcp`) exposing the full code-map +
  git-history tool surface (`outline`, `search_symbols`, `find_references`,
  `find_callers`, `list_files`, `dependents`, `repo_info`, `status`, `symbol_history`,
  `working_tree_status`, `recent_changes`, `commits_touching`, `find_commits_by_path`,
  `diff_file`, `diff_outline`, `hot_files`, `blame_file`, `blame_symbol`).
- **Dynamic 300+ language coverage** via
  [tree-sitter-language-pack](https://github.com/kreuzberg-dev/tree-sitter-language-pack).
  Hand-written `.scm` overrides ship for Rust, Python, TypeScript, TSX, JavaScript, Go.
  Any other language for which TSLP ships a vendored `tags.scm` (kotlin, csharp, swift,
  cpp, scala, solidity, lua, …) gets best-effort symbol + call extraction via the
  fallback adapter that rewrites GitHub-standard `@definition.*` / `@reference.call`
  captures into gitmind's `@symbol.*` / `@call.*` shape.
- **Real-OSS hardening harness** (`tests/harden.rs`, `./scripts/harden.sh`) — clones 8
  upstream repos (ripgrep, tokio, typescript, react, django, requests, gin,
  ripgrep-shallow), exercises every MCP tool against each, and pins canary lower bounds
  (tokio: `find_references("spawn") >= 50`, django: `find_references("get") >= 50`,
  react: `search_symbols("useState") >= 20`).
- **Schema sync to release version** — `RELEASE_MINOR` in `src/version.rs` drives both
  `INDEX_SCHEMA_VER` and the blob `SCHEMA_VER`. Minor-release bumps wipe `.gitmind/`
  on next scan; patch releases stay compatible.
- Distribution: `brew install Goldziher/tap/gitmind`, `npm install -g gitmind`,
  `pip install gitmind`, `cargo install gitmind --locked`. Precompiled binaries on
  GitHub Releases for `{x86_64,aarch64}-{linux-gnu,apple-darwin}` and
  `x86_64-pc-windows-gnu`.

[Unreleased]: https://github.com/Goldziher/gitmind/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Goldziher/gitmind/releases/tag/v0.1.0
