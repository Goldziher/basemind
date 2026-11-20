# Contributing to basemind

Thanks for taking the time to contribute! basemind is a pure-Rust project with a
small surface area and a sharp commit-quality bar.

## Quickstart

```sh
git clone https://github.com/Goldziher/basemind && cd basemind
mise run setup     # mise install + cargo fetch + poly hook install (one-time)
mise run check     # lint + test
```

You'll need:

- Rust 2024 edition (stable)
- [`mise`](https://mise.jdx.dev) — toolchain and task runner; `mise install` provides poly, Node, pnpm, Python
  and uv at the versions in `mise.toml`

## Pre-commit hooks

Install the git hooks with `mise run setup` (or `mise run governance:hooks:install` directly). On
every commit, poly runs lint, format, and file-safety checks plus `cargo clippy`;
the commit-msg hook validates the message. Run all hooks manually with
`poly hooks run pre-commit --all-files`.

## Workflow

1. Open an issue or comment on an existing one before starting non-trivial work.
2. Branch from `main`. Keep branches short-lived; rebase if drift gets ugly.
3. Write tests first when the change is observable from the public API or MCP
   surface. RED → GREEN → REFACTOR.
4. Run `mise run check` before pushing — `poly` runs the same hooks locally that CI
   runs.
5. Open a PR. Conventional Commit prefix in the title:
   - `feat:` new functionality
   - `fix:` bug fix
   - `perf:` performance improvement
   - `refactor:` code change without behavior change
   - `chore:` tooling / housekeeping
   - `docs:` documentation only
   - `test:` test only

## Code style

- Rust 2024 edition, `cargo fmt`, `clippy -D warnings`.
- `Result<T, E>` with `thiserror` for libraries, `anyhow` for app paths. `?` for
  propagation — no `unwrap()` in library code.
- Prefer `&str` / `&[u8]` in arguments; defer the owned clone to the boundary.
- See `.ai-rulez/rules/` for the full set of enforced project conventions.

## Performance

basemind is a hot-path scanner. Before merging diffs that touch
`src/scanner.rs`, `src/extract/`, `src/store.rs`, `src/index/`, or
`src/engine/`:

- Use `ahash::AHashMap` / `AHashSet`, never `std::collections::HashMap` on the
  scanner / extract / index paths.
- Use `memchr::memmem::Finder` for substring matching, not `str::contains`.
- Reuse the tree-sitter parser pool; never construct a parser per file.
- Run the harden harness if the change is non-trivial:
  `./scripts/harden.sh` (`#[ignore]`-gated, takes ~10 minutes).

## Adding a language

See [`.ai-rulez/skills/language-support/SKILL.md`](.ai-rulez/skills/language-support/SKILL.md)
for the end-to-end checklist. The short version: drop a hand-written extraction
query at `src/queries/<pack-name>.scm` with `;; section: symbols / imports / calls / docs`
sections; the override wins over the upstream `tags.scm` fallback. Prose, data and config grammars
(markdown, json, yaml, toml, ...) are not code: `lang::is_non_code` routes them to the document tier,
so a new grammar of that kind belongs in that list rather than getting a code-map query.

## Adding an MCP tool

See [`.ai-rulez/skills/mcp-tool-checklist/SKILL.md`](.ai-rulez/skills/mcp-tool-checklist/SKILL.md).
Every new tool needs: a `<Tool>Params` in `src/mcp/types_tools.rs`, a thin shim
in `src/mcp/tools.rs` (or `tools_<area>.rs`), its body as an engine operation in
`src/engine/ops/<tool>.rs` with its response in `src/engine/types/`, a smoke
assertion in `tests/mcp_smoke.rs`, and (when meaningful) a per-repo canary in
`tests/harden.rs`.

## Schema bumps

Any change to the on-disk schema (msgpack blob layout or Fjall keyspace) bumps
the relevant constant:

- `INDEX_SCHEMA_VER` in `src/index/mod.rs` — Fjall partition / key encoding
- `SCHEMA_VER` in `src/extract/mod.rs` — msgpack blob format

Both track the release minor and auto-reset on version mismatch; the next `basemind scan` rebuilds from
source. Mention the bump in the commit body. Never bump either from a patch release: if an extractor
fix changes what an unchanged file's blob should contain but not its serialized shape, bump
`EXTRACT_EPOCH` in `src/extract/mod.rs` instead, and the next scan re-extracts only the stale files. A
brand-new Fjall keyspace whose absence reads as "nothing known" needs no bump. When you add an on-disk
component, follow the checklist in [`docs/UPGRADING.md`](docs/UPGRADING.md) (blob suffixes, cache
clear/stats, `tests/upgrade_from_release.rs`).

## Plugin and packages

Product sources live in `plugin/.ai-rulez/`; root `.ai-rulez/` governs development. Runtime manifests,
packages, marketplace indexes, and skill mirrors are generated from these sources: regenerate with
`mise run plugin:generate` and verify with `mise run plugin:verify` before submitting changes, and
never edit generated runtime files directly. Gemini context lives at `plugin/assets/GEMINI.md`.
Run `mise run plugin:hermes:native:test` with `HERMES_PYTHON` pointing to the Hermes SDK interpreter
to test the Hermes plugin against Hermes itself.

`mise run plugin:generate` also mirrors the canonical `.pi/extensions/basemind.ts` and the
product skills into `opencode-plugin/`. `mise run package:plugin` stages the combined package from
canonical sources and runs a real pnpm pack without changing the checkout. The `publish_opencode`
job checks the ten skills, the Pi extension and both OpenCode adapters before publishing the packed
archive; Pi needs no separate registry package.

Publishing verifies the packed npm archive, the Hermes wheel and the all-runtime archive before
checking whether a version already exists. The manual artifact workflow packages the selected
checkout only when its core and plugin versions match the requested existing release tag.
Marketplace indexes pin that checkout's immutable commit; the archive attaches to the requested
release. It does not create a product tag or publish registry packages.

A new registry package needs a first manual publish and a trusted publisher. For the npm agent
package `@goldziher/basemind-plugin`, configure it with
`npm trust github @goldziher/basemind-plugin --repo Goldziher/basemind --file publish.yaml --allow-publish`;
without it the OIDC token exchange returns 404 and the publish fails. Hermes publishing needs its
PyPI trusted publisher. Configure those account settings before requesting a release.

## Changelog

User-visible changes get an entry under `## [Unreleased]` in `CHANGELOG.md`, grouped under `### Added`,
`### Changed` and `### Fixed` (one heading of each kind, no duplicates). Use measured numbers only,
with the corpus they were measured on.

## Reporting bugs

[Open an issue](https://github.com/Goldziher/basemind/issues/new) with:

- basemind version (`basemind --version`)
- Repro steps
- OS / arch
- A pointer to the repo where it reproduces, if public

## License

By contributing, you agree your contributions are licensed under the [MIT
License](LICENSE).
