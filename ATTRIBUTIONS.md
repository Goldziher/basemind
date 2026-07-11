# Attributions

This document acknowledges sources of forked/copied code and rule data used in the basemind
project. Ordinary crates.io dependencies are tracked in `Cargo.toml` / `Cargo.lock` and governed by
`deny.toml`; this file covers code and data files that were **copied into the repository** — either
rule data or workspace crates under `crates/` that basemind now maintains as forks.

## stack-graphs name-binding rules (`.tsg`)

Tree-sitter-graph (`.tsg`) name-resolution rule files driving basemind's precise, scope- and
import-aware navigation for Python and Java.

- **Source**: <https://github.com/github/stack-graphs> (archived, read-only)
- **Upstream paths**:
  - `languages/tree-sitter-stack-graphs-python/src/stack-graphs.tsg`
  - `languages/tree-sitter-stack-graphs-java/src/stack-graphs.tsg`
- **Upstream commit**: `fcb7705d5b38ae13b3665a9b2c882e5a97243d44`
- **License**: MIT OR Apache-2.0
- **Authors**: GitHub, Inc. and the stack-graphs contributors
- **Location**: `src/intel/tsg/python.tsg`, `src/intel/tsg/java.tsg`
- **Purpose**: Declarative name-binding rules executed against tree-sitter parse trees to build a
  scope/definition/reference stack graph, resolved intra-file to precise use→definition edges.

### Modifications

- Derived from the upstream rule files (copyright header preserved) and now **maintained** by
  basemind. A `;;`-comment attribution header (source, commit, license, target grammar version) is
  prepended to each file.
- The rules were written against `tree-sitter-python =0.23.5` / `tree-sitter-java =0.23.4`; basemind
  parses via `tree-sitter-language-pack` 1.12.5. Grammar-drift adaptations for node types that do not
  exist in the current grammar (e.g. Python's `except_group_clause`) are stripped at engine-build
  time in `src/intel/stackgraph.rs`.
- Rule bug fixes for valid modern constructs are applied **directly to the rule files**. Notably the
  Python `typed_parameter` stanza was restricted to identifier-named parameters
  (`. (identifier) @name`) so a *typed* splat parameter (`**kwargs: T` / `*args: T`) no longer aborts
  the whole stack-graph build — the upstream rule captured the splat pattern as a plain name and
  failed on its undefined `.def`, silently losing all resolution for any file using typed splats.

### License Compatibility

MIT OR Apache-2.0 is permissive and compatible with basemind's MIT license.

---

## tree-sitter-graph (forked & maintained)

The tree-sitter-graph DSL interpreter — parses a `.tsg` file and executes it against a tree-sitter
parse tree to construct a graph. basemind maintains this as a **fork**: a first-class workspace
crate that we own and modernize (not a throwaway vendored copy).

- **Originally derived from**: <https://github.com/tree-sitter/tree-sitter-graph> v0.12.0
- **License**: MIT OR Apache-2.0 (upstream LICENSE-MIT / LICENSE-APACHE retained in the crate)
- **Original authors**: Douglas Creager and the tree-sitter-graph contributors
- **Now maintained by**: basemind (Na'aman Hirschfeld)
- **Location**: `crates/tree-sitter-graph/`
- **Purpose**: Execute the stack-graphs `.tsg` rules against basemind's tree-sitter parse trees.

### Modifications

- Ported from `tree-sitter ^0.24` to `tree-sitter 0.26` (the version basemind uses via
  tree-sitter-language-pack): streaming `QueryCursor` iteration via the `streaming-iterator` crate,
  `&Language` API signatures, and related 0.25/0.26 binding changes.
- CLI / `clap` entry points and the `tree-sitter-loader` / `tree-sitter-config` optional integrations
  were dropped — basemind supplies its own already-parsed trees and needs only the interpreter.
- Modernized as an owned fork: promoted from `vendor/` to a `crates/` workspace member, bumped to
  Rust edition 2024, and made clippy-clean under the workspace `-D warnings` bar (removed the dead
  `term-colors` feature branches, elided/precise-captured lifetimes, idiom cleanups).

### License Compatibility

MIT OR Apache-2.0 is compatible with basemind's MIT license. Upstream LICENSE files are preserved in
the fork crate.

---

## tree-sitter-stack-graphs (forked & maintained)

The thin builder that maps a tree-sitter-graph execution result into a `stack_graphs::StackGraph`
(the `.tsg` special globals + node-attribute conventions). basemind maintains this as a **fork**
alongside tree-sitter-graph — a first-class workspace crate we own and modernize.

- **Originally derived from**: <https://github.com/github/stack-graphs> v0.10.0 (archived, read-only)
- **License**: MIT OR Apache-2.0 (upstream LICENSE-MIT / LICENSE-APACHE retained in the crate)
- **Original authors**: GitHub, Inc. and the stack-graphs contributors
- **Now maintained by**: basemind (Na'aman Hirschfeld)
- **Location**: `crates/tree-sitter-stack-graphs/`
- **Purpose**: Build a `StackGraph` from a `.tsg` rule set + a parse tree, so the `stack-graphs`
  path-stitcher can resolve references to definitions.

### Modifications

- Ported to `tree-sitter 0.26` and the forked `tree-sitter-graph` above.
- Reduced to the `.tsg`→`StackGraph` build path; the CLI, LSP, test-runner, and per-language
  loader/config machinery were dropped.
- Modernized as an owned fork: promoted from `vendor/` to a `crates/` workspace member, bumped to
  Rust edition 2024, and made clippy-clean under the workspace `-D warnings` bar. The
  `parse_with_options` + progress-callback (cancellation) port is preserved as-is.

### License Compatibility

MIT OR Apache-2.0 is compatible with basemind's MIT license.

---

## lsp-positions (forked & maintained)

LSP-compatible character positions (UTF-8 / UTF-16 / grapheme offsets). Shared by the forked
tree-sitter-stack-graphs and the crates.io `stack-graphs` dependency; both must resolve to this one
instance (via `[patch.crates-io]`) so their `lsp_positions::Span` types unify. basemind maintains it
as a **fork** because the published crate pins tree-sitter 0.24, incompatible with basemind's 0.26.

- **Originally derived from**: <https://github.com/github/stack-graphs> lsp-positions v0.3.4
  (archived, read-only)
- **License**: MIT OR Apache-2.0 (upstream LICENSE-MIT / LICENSE-APACHE retained in the crate)
- **Original authors**: GitHub, Inc. and the stack-graphs contributors
- **Now maintained by**: basemind (Na'aman Hirschfeld)
- **Location**: `crates/lsp-positions/`
- **Purpose**: Provide the `Span` / `Position` / `Offset` position model used across the stack-graph
  build path.

### Modifications

- Ported to `tree-sitter 0.26`; the `tree-sitter` feature is the only one basemind enables (the
  `bincode` / `serde` features are retained but off by default).
- Modernized as an owned fork: promoted from `vendor/` to a `crates/` workspace member, bumped to
  Rust edition 2024, and made clippy-clean under the workspace `-D warnings` bar.

### License Compatibility

MIT OR Apache-2.0 is compatible with basemind's MIT license.

---

## stack-graphs

The `stack-graphs` graph model and `ForwardPartialPathStitcher` path-finding are consumed as an
ordinary crates.io dependency (`stack-graphs = { version = "0.14", default-features = false }`), not
vendored — it has no tree-sitter dependency and its optional `storage` (rusqlite) feature is left
disabled. It is noted here only because its upstream repository (<https://github.com/github/stack-graphs>)
is archived; the crate remains published under MIT OR Apache-2.0.
