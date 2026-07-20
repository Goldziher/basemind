# Contributing to basemind

Thanks for taking the time to contribute! basemind is a pure-Rust project with a
small surface area and a sharp commit-quality bar.

## Quickstart

```sh
git clone https://github.com/Goldziher/basemind && cd basemind
task setup     # cargo fetch + poly hook install (one-time)
task check     # lint + test
```

You'll need:

- Rust 2024 edition (stable)
- [`task`](https://taskfile.dev) — task runner
- [`poly`](https://github.com/Goldziher/poly) — lint and git-hook runner (installed by `task setup`)

## Pre-commit hooks

Install the git hooks with `task setup` (or `poly hooks install` directly). On
every commit, poly runs lint, format, and file-safety checks plus `cargo clippy`;
the commit-msg hook validates the message. Run all hooks manually with
`poly hooks run pre-commit --all-files`.

## Workflow

1. Open an issue or comment on an existing one before starting non-trivial work.
2. Branch from `main`. Keep branches short-lived; rebase if drift gets ugly.
3. Write tests first when the change is observable from the public API or MCP
   surface. RED → GREEN → REFACTOR.
4. Run `task check` before pushing — `poly` runs the same hooks locally that CI
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
`src/mcp/helpers.rs`:

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
sections; the override wins over the upstream `tags.scm` fallback.

## Adding an MCP tool

See [`.ai-rulez/skills/mcp-tool-checklist/SKILL.md`](.ai-rulez/skills/mcp-tool-checklist/SKILL.md).
Every new tool needs: a `<Tool>Params` / `<Tool>Response` in `src/mcp/types.rs`
(or the matching `types_<area>.rs`), a thin shim in `src/mcp/tools.rs` (or
`tools_<area>.rs`), a helper body in the matching `src/mcp/helpers*.rs`, a
smoke assertion in `tests/mcp_smoke.rs`, and (when meaningful) a per-repo
canary in `tests/harden.rs`.

## Schema bumps

Any change to the on-disk schema (msgpack blob layout or Fjall keyspace) bumps
the relevant constant:

- `INDEX_SCHEMA_VER` in `src/index/mod.rs` — Fjall partition / key encoding
- `SCHEMA_VER` in `src/extract/mod.rs` — msgpack blob format

Both auto-wipe on version mismatch; the next `basemind scan` rebuilds from
source. Mention the bump in the commit body.

## Reporting bugs

[Open an issue](https://github.com/Goldziher/basemind/issues/new) with:

- basemind version (`basemind --version`)
- Repro steps
- OS / arch
- A pointer to the repo where it reproduces, if public

## License

By contributing, you agree your contributions are licensed under the [MIT
License](LICENSE).
