# ADR-0016: Shared base index with per-worktree overlays

- **Status:** Proposed
- **Date:** 2026-10-18
- **Deciders:** Na'aman Hirschfeld
- **Related:** ADR-0012, issue #87

## Context

Each git worktree is its own workspace: the store key is blake3 of the canonical root
(`src/store_layout.rs`), so every worktree gets a full Fjall index. On a large monorepo (89k files, 65k
indexed) one working-view index is ~2.9 GB, of which the Fjall is 2.6–2.8 GB; only the content-addressed
blobs and `git-history.fjall` are shared. `store_seed.rs` clones a sibling's `views/working` with
clonefile, but compaction rewrites the pages, so the copies drift into full copies. With 33 worktrees that
is ~95 GB, and every cold worktree costs a 17–33 min scan peaking at 6–7 GB RSS. The per-worktree disk and
scan cost should be proportional to the worktree's diff, with query results identical to a full index.

## Decision

Group worktrees by blake3 of the canonical git common dir and split the index into an immutable **base**
and a per-worktree **overlay**:

- **Base** — an index of `origin/HEAD` (falling back to the main worktree HEAD) built from git objects
  through the existing rev scan, so it never contains uncommitted edits. Generations advance by a CoW
  clone plus `git diff-tree C_n..C_n+1` applied through the incremental writer when files have drifted by
  2% or more, or daily; at most three exist, each published read-only so the daemon keeps one handle and
  one block cache per group.
- **Overlay** — `delta.msgpack` records each path as added, modified or deleted relative to the base
  manifest by content hash; `overlay.fjall` holds rows only for changed paths and re-stitched importers.
  Shadow sets `S_path` (delta paths), `S_refs` (`S_path` plus re-stitched importers) and `D_hash` (base
  hashes no longer live in the worktree) drive the merge.
- **Read merge** — every per-name keyspace already carries the path in its key, so the overlay merges with
  the base preserving order, totals and cursors: `*_by_path` take the overlay for a path in `S_path`;
  `symbols_by_name`, `calls_by_callee`, `imports_by_module` and `implementations_by_trait` drop base rows
  whose path is shadowed; `refs_by_*` drop base rows whose use-path is in `S_refs`;
  `code_bm25_postings` drop base rows whose hash is in `D_hash`; the BM25 `meta` is base + overlay −
  shadowed `code_bm25_by_path`. `grep_bloom` is re-keyed by content hash and shared.
- **Write path** — candidates are `git diff --name-only C_base` ∪ untracked files ∪ basemind-only files;
  only candidates are hashed, and blobs are reused. An `IndexView` trait with standalone and layered
  implementations exposes `prefix`/`range`/`get`/`iter`, and `IndexRoute` / `index_read_against` take
  `&dyn IndexView`. If the delta exceeds 15% of files or 25% of bytes the overlay rebases onto a newer
  generation or is promoted to a standalone index (today's model).
- **Lance** — tables are shared per group: `code_chunks` is already content-addressed and
  `documents`/`doc_links` are re-keyed to `(scope, content_hash)`, with queries over-fetching and filtering
  by live hashes. Documented deviation: top-k overlap with a per-worktree table stays ≥0.95.

## Consequences

Per-worktree disk and scan cost become proportional to the diff, and one block cache serves a whole group.
The read path grows a merge layer that must preserve order, totals and cursors for every mode; the
correctness gate (a proptest over three worktrees with random edits, comparing every mode's results and
cursors to a standalone full index) exists to catch a merge that does not. `store_seed.rs` is removed once
layered mode is the default. Phases A0 (per-repo memory store) and A1 (probe index and O(affected)
re-stitch) shipped in 0.30.0; A2–A8 remain.

## Alternatives considered

- **Index only the diffs and query git for the rest.** Rejected: git has no symbols, refs or BM25.
- **Deduplicate full copies.** Rejected: compaction breaks the sharing.
- **One shared Fjall with path-prefixed keys.** Rejected: name scans multiply by the worktree count, and
  one lock covers the whole group.
