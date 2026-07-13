# stack-graphs (maintained basemind fork)

Name binding for arbitrary programming languages: the stack-graph data model (`graph`), partial
paths (`partial`), and the forward partial-path stitcher (`stitching`) that basemind's code-intel
pass uses to resolve references to definitions.

## Provenance

- **Originally derived from**: <https://github.com/github/stack-graphs> (archived, read-only), the
  published `stack-graphs` crate v0.14.1.
- **Upstream authors**: GitHub, Inc. (`opensource+stack-graphs@github.com`), Douglas Creager, and
  the stack-graphs contributors. Upstream copyright headers are retained in every source file.
- **License**: MIT OR Apache-2.0 (`LICENSE-MIT` / `LICENSE-APACHE`), unchanged from upstream.
- **Now maintained by**: basemind. Upstream is archived, so bug fixes land here.

## Modifications

- **Edition 2024**, clippy-clean under the workspace `-D warnings` bar.
- `lsp-positions` is a workspace path dep (`crates/lsp-positions`), so the graph, the forked
  `tree-sitter-stack-graphs`, and this crate all share one `lsp_positions::Span` type.
- Stripped: the C FFI (`src/c.rs`, `include/stack-graphs.h`, cbindgen), the `serde`,
  `visualization`, and `storage` (rusqlite/bincode) modules. basemind builds a `StackGraph` in
  memory and stitches it in-process — none of that surface is reachable.
- **Two upstream panics on the stitching hot path were made recoverable.** A panic while resolving
  one file must never abort a scan of an 82k-file repository:
  - `Database::get_incoming_path_degree` (`src/stitching.rs`) indexed the lazily-grown
    `incoming_paths` supplemental arena directly, which panics for any node that is not the end node
    of some partial path already in the database. It now reports `Degree::Zero` for such nodes.
  - `ForwardPartialPathStitcher::extend` (`src/stitching.rs`) called `.expect()` on the cycle
    detector. The cycle test replays a *suffix* of the path against freshly minted stack variables,
    which can legitimately fail to unify (`ScopeStackUnsatisfied`). An undecidable cycle test now
    discontinues the path — the same conservative outcome as a detected cycle — instead of panicking.

  Both are covered by regression tests in `tests/it/stitching.rs`.
