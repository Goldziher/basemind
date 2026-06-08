---
priority: medium
description: "Adding tree-sitter language coverage"
---

# Language Support

Use this to add a new language to basemind's extraction. Supported languages today include Rust, Python, TypeScript/JSX, JavaScript, Go.

## Inputs needed

- A tree-sitter grammar crate (e.g. `tree-sitter-elixir`) reachable via the `tree-sitter-language-pack` umbrella crate basemind already depends on, or as a direct dep.
- The grammar's upstream `queries/tags.scm` from the tree-sitter org repo — it's the canonical starting point for symbol + reference extraction.

## Steps

1. **Register the language**
   - Add the lang ID + file extensions to `src/lang.rs`'s language registry.
   - Add the extensions to the scanner's accepted-extension set in `src/scanner.rs` (or the central glob if one exists).

2. **L1 (outline) queries**
   - Adapt the upstream `tags.scm` to basemind's L1 capture shape: `@symbol.function`, `@symbol.method`, `@symbol.class`, etc. mapped to `extract::SymbolKind`.
   - Place query string in `src/queries/<lang>.rs` (or under `src/extract/queries/`, matching the existing layout).
   - Wire into `extract::l1::extract`'s language dispatch.

3. **L2 (calls) queries**
   - Add a call query: callees as `@call.callee` plus optional receiver / type info.
   - Wire into `extract::l2::extract`'s dispatch. Remember to populate `start_row` / `start_col` on `Call`.

4. **Fixtures + smoke test**
   - Add a synthetic file under `tests/fixtures/<lang>/` exercising at least: one function def, one method, one call site, one import.
   - Add assertions in `tests/mcp_smoke.rs` (or a new `tests/<lang>_smoke.rs`) covering outline + find_references for the fixture.

5. **Harden harness canary**
   - Pick a real OSS repo for the language (criteria: stable, ≥ a few hundred files, well-known canary symbols). Add to the repo list in `tests/harden.rs` with a clone URL + optional `--depth`.
   - Add a `>= N` canary assertion (`find_references` or `search_symbols`) — see the `harness-canary-authoring` skill.

6. **README**
   - Update the supported-languages list with the new language and file extensions.

## Pitfalls

- Tree-sitter queries are language-specific; do not copy capture names blindly between grammars (e.g. `function.definition` vs `function_definition`). Test against the fixture.
- Don't forget the file-extension glob — extraction is silently skipped otherwise and the symptom is "tool returns 0 hits for a known symbol."
- Eager L2 must be enabled (default) for `find_references` to populate on the new language.
