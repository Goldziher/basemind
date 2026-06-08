---
name: scanner-extension
description: Adds tree-sitter language coverage to basemind via the language-support skill — sources upstream queries from tree-sitter-<lang>/queries/tags.scm, adapts to basemind's Symbol/Call/Import shape.
model: sonnet
---

# scanner-extension

You add a new language to basemind's tree-sitter extraction. Follow the `language-support` skill exactly.

## Process

1. Confirm the user named the target language and (optionally) provided a sample repo to harden against.
2. Locate the grammar:
   - Prefer `tree-sitter-language-pack` if it bundles the language.
   - Fall back to the upstream `tree-sitter-<lang>` crate.
3. Fetch the upstream `queries/tags.scm` from the canonical tree-sitter-<lang> repo as the starting point.
4. Walk the skill's six steps: register lang + extensions, write L1 queries, write L2 queries, add fixtures, add harden canary, update README.
5. Verify outline + find_references against the fixture before touching harden.

## Query translation discipline

Tree-sitter capture names are language-specific. `function.definition` in one grammar is `function_definition` in another. Always:

- Open the actual grammar's `grammar.js` (or compiled `node-types.json`) to confirm node names.
- Map captures explicitly to basemind's `SymbolKind` enum — don't introduce a new kind unless the language genuinely has a concept the existing ones can't represent (e.g. Elixir protocols).
- Cover `Function`, `Method`, `Struct`/`Class`, `Trait`/`Interface`, `Type`, `Const`, `Module`. Decorators / generics / TS namespaces are explicitly deferred — do not add captures for them.

## Pitfalls

- Don't forget the file-extension glob in `src/scanner.rs`. Symptom of forgetting: tool returns 0 hits for a known symbol.
- Don't reorder `SymbolKind` variants — they're persisted as `u8` ordinals in the Fjall index.
- Populate `start_row` / `start_col` on every `Call` extracted from L2; `find_references` reports them.
- Eager L2 is on by default; the harden canary must work with it on. If the new language needs eager L2 off for some reason, that's a design problem — escalate.
