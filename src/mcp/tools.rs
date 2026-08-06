//! The `code` domain tool shim for `BasemindServer`.
//!
//! One tool, one required `mode` — `outline` / `symbols` / `grep` / `files` / `find` /
//! `definition` / `references` / `callers` / `implementations` / `dependents` / `expand` /
//! `semantic` / `chunk` — dispatched to `helpers_code::run_code`. Thin wrapper: the bodies live in
//! `helpers_code.rs` (`outline`, `symbols`, `dependents`), `helpers_files.rs`, `helpers_grep.rs`,
//! `helpers_calls.rs`, `helpers_impls.rs`, `helpers_intel.rs`, `helpers_compress.rs` and
//! `helpers_code_search.rs`.
//!
//! The pagination / cap helpers the code-map bodies share stay here at the bottom of the file.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;
use serde_json::Value;

use super::BasemindServer;
use super::helpers::{LIST_LIMIT_DEFAULT, LIST_LIMIT_MAX, record_call};
use super::lenient::Lenient;
use super::mode::CodeMode;
use super::types::{FindCallersParams, FindReferencesParams, OutlineParams, SearchSymbolsParams, WorkspaceGrepParams};
use super::types_code::CodeParams;

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_core")]
impl BasemindServer {
    // No `output_schema`: the thirteen modes return thirteen different response shapes, and
    // SEP-2106 allows exactly one per tool. Declaring a union would mean nested structs, which
    // schemars emits as `$ref` into `$defs` — the construct that silently dropped the whole
    // registry in GH #50. The per-mode shapes are documented in the description instead. ~keep
    #[tool(
        description = "Read this repository's code map instead of opening files: grep it, find \
        where something is defined, see who calls this, locate a file by name, and pull one \
        symbol's body. Everything is served from the index — paths, lines, columns and \
        signatures — at a fraction of the tokens a Read or a shell `rg` costs. `mode` is \
        required. `outline` returns one file's structure (symbols with kind + row/col + \
        signature, plus imports) — read this INSTEAD of opening the file, then fetch only the \
        span you need; `l2: true` adds call sites and doc comments when an L2 blob exists for \
        the file's current content. `symbols` searches every indexed symbol NAME for `name` as \
        a case-sensitive SUBSTRING (optional `kind` filter) — the definition finder, and the \
        right answer to \"where is X defined\"; `total` counts matches up to a per-call cap \
        (`limit*64`, min 2000) and sets `total_is_partial` when it stops there. `grep` is regex \
        content search (Rust `regex` syntax) over EVERY indexed file — use it for a pattern, \
        a string literal or a comment, and prefer `symbols` for a plain identifier; `limit` \
        caps hits, never files, so `total_matches` is exact, and `language` / `path_contains` \
        narrow the sweep. `files` enumerates indexed paths (`path_contains` / `language` \
        filters). `find` is fuzzy filename search (fzf/fd-style subsequence, case-insensitive, \
        ranked by score) — reach for it instead of `find` / `fd` / `ls -R`. `definition` \
        resolves the reference at `path`:`line`:`column` to the definition it BINDS to — \
        scope-resolved, not name-matched, so it never conflates same-named symbols, and it \
        follows cross-file imports for JS/TS; `line` is 1-based, `column` 0-based bytes; a \
        position with no resolved binding returns no `definition` rather than an error. \
        `references` finds every call site whose callee identifier contains `name` — NAME-ONLY, \
        no scope resolution, so `Foo::bar()` and `bar()` both match `name=\"bar\"`; that is the \
        fast, complete floor for \"what calls this\". `callers` answers the same question for \
        ONE specific definition: it resolves `path` + `name` (+ optional `kind`) first, echoes \
        it as `definition`, then runs that same name scan — so `total` agrees with `references` \
        on an unambiguous name — and additionally marks each hit `resolved` when scope/import \
        resolution PROVED it binds to that definition (`resolved_total`). `resolved: false` is \
        not evidence a hit isn't a caller; trust `total` for completeness before a refactor and \
        filter on `resolved` when you want precision. `implementations` finds the types that \
        implement / extend / inherit `trait_name` (case-sensitive substring; Rust, Python, \
        TS/TSX, JS class + interface extends/implements — Go structural satisfaction is not \
        detected). `dependents` is the reverse import lookup: indexed files whose imports \
        mention `module` (heuristic substring against each recorded module path). `expand` \
        returns one symbol's RAW SOURCE BODY resolved by `path` + `name` (+ `kind` to \
        disambiguate an overload) — the inverse of an outline entry, and the second half of the \
        outline-then-expand pattern. `semantic` searches code by MEANING rather than spelling: \
        `lane` picks `hybrid` (default; RRF fusion of vector, BM25 and exact-symbol lanes, \
        degrading gracefully when a lane is unavailable), `semantic` (vector only) or `keyword` \
        (BM25 only); it returns POINTERS (path + line/byte range + symbol), never bodies — \
        fetch one with `chunk`. `chunk` returns a single chunk's source by `path`, \
        disambiguated with `chunk_id` or `byte_start`. Caps: `symbols` / `grep` / `references` \
        / `callers` / `implementations` default `limit` 100, max 1000; `files` / `find` default \
        200, max 5000; `semantic` default 10, max 100. The index scanners bound their work at \
        `scan_cap = limit * 8` and flag a cut result with `total_is_partial`. `cursor` pages \
        (`references` / `callers` / `implementations` cursors are stable across rescans; the \
        in-memory ones set `cursor_invalidated`), `max_tokens` budgets the returned list and \
        sets `budgeted`, and `format: \"toon\"` returns compact tabular rows. Parameters that \
        belong to another mode are rejected, not ignored.",
        // Every mode is a read over the local index or working tree — nothing is written and
        // nothing outside this repository is contacted, so the union of the thirteen keeps the
        // fully-pure hints a client may auto-approve on. ~keep
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn code(
        &self,
        Parameters(Lenient(p)): Parameters<Lenient<CodeParams>>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __key = p.mode.telemetry_key();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = super::helpers_code::run_code(&self.state, p).await;
        record_call(&self.state, __key, &__params_json, __started, &__result);
        __result
    }
}

/// Named in-process entry points for the five code-map operations the `basemind-agent` engine
/// exposes as its own tools, bridged through [`super::agent_api`].
///
/// They are deliberately NOT `#[tool]`s: the MCP registry advertises exactly one `code` tool. Each
/// builds the same [`CodeParams`] an MCP caller would send, so telemetry, validation, and the
/// response shape are identical to `code { mode: … }`.
impl BasemindServer {
    pub(crate) async fn outline(
        &self,
        Parameters(Lenient(p)): Parameters<Lenient<OutlineParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.code(Parameters(Lenient(CodeParams {
            path: Some(p.path),
            l2: Some(p.l2),
            max_tokens: p.max_tokens,
            format: p.format,
            ..CodeParams::new(CodeMode::Outline)
        })))
        .await
    }

    pub(crate) async fn search_symbols(
        &self,
        Parameters(Lenient(p)): Parameters<Lenient<SearchSymbolsParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.code(Parameters(Lenient(CodeParams {
            name: Some(p.needle),
            kind: p.kind,
            limit: p.limit,
            max_tokens: p.max_tokens,
            format: p.format,
            cursor: p.cursor,
            ..CodeParams::new(CodeMode::Symbols)
        })))
        .await
    }

    pub(crate) async fn find_references(
        &self,
        Parameters(Lenient(p)): Parameters<Lenient<FindReferencesParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.code(Parameters(Lenient(CodeParams {
            name: Some(p.name),
            limit: p.limit,
            max_tokens: p.max_tokens,
            format: p.format,
            cursor: p.cursor,
            ..CodeParams::new(CodeMode::References)
        })))
        .await
    }

    pub(crate) async fn find_callers(
        &self,
        Parameters(Lenient(p)): Parameters<Lenient<FindCallersParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.code(Parameters(Lenient(CodeParams {
            path: Some(p.path),
            name: Some(p.name),
            kind: p.kind,
            limit: p.limit,
            max_tokens: p.max_tokens,
            cursor: p.cursor,
            ..CodeParams::new(CodeMode::Callers)
        })))
        .await
    }

    pub(crate) async fn workspace_grep(
        &self,
        Parameters(Lenient(p)): Parameters<Lenient<WorkspaceGrepParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.code(Parameters(Lenient(CodeParams {
            pattern: Some(p.pattern),
            language: p.language,
            path_contains: p.path_contains,
            limit: p.limit,
            max_tokens: p.max_tokens,
            format: p.format,
            include_context: Some(p.include_context),
            cursor: p.cursor,
            ..CodeParams::new(CodeMode::Grep)
        })))
        .await
    }
}

/// Resolve the effective `list_files` page limit and report whether the caller's
/// requested limit was clamped to [`LIST_LIMIT_MAX`].
///
/// Returns `(effective_limit, clamped)` where `clamped` is true iff the caller asked
/// for more than the cap allows — surfaced honestly to the client rather than silently
/// truncating (bug #17).
pub(super) fn effective_list_limit(requested: Option<u32>) -> (usize, bool) {
    let asked = requested.unwrap_or(LIST_LIMIT_DEFAULT);
    let clamped = asked > LIST_LIMIT_MAX;
    (asked.min(LIST_LIMIT_MAX) as usize, clamped)
}

/// The `code` mode `symbols` scan cap: matches walked are bounded by this so a common needle
/// never scans the whole corpus. When the cap is reached, `total` is a lower bound, not the
/// global match count — the response sets `total_is_partial` so callers don't mistake it for
/// a true total (bug #16).
pub(super) fn search_max_total(limit: usize) -> usize {
    limit.saturating_mul(64).max(2_000)
}

/// Count content-addressed blobs in the GLOBAL blob store by tallying `.fm.msgpack` files
/// (one combined L1 + L2 filemap per indexed content hash; the `.doc` sibling shares the same
/// stem so it is not double-counted). A single directory read — cheaper than
/// [`crate::store_gc::cache_stats`], which also unions every view index — so it is safe to call
/// from the `status` path.
///
/// Since the blob store went machine-global this counts blobs across EVERY workspace, so the
/// `status` divergence heuristic (empty index yet blobs on disk) can over-report on a shared
/// machine — advisory only, as before.
///
/// Returns `0` when the blobs directory is absent or unreadable; the count is advisory.
pub(super) fn count_fm_blobs() -> usize {
    crate::store::count_fm_blobs(&crate::store::global_blobs_dir())
}

/// Build the `status` divergence note (bug #10): when the current view's index is empty
/// but content-addressed blobs are present on disk, the index was lost/wiped over live
/// blobs and a rescan would rebuild it. A legitimately unscanned repo (no blobs either)
/// gets no note.
pub(super) fn blob_divergence_note(file_count: usize, blob_count: usize) -> Option<String> {
    if file_count == 0 && blob_count > 0 {
        Some(format!(
            "index for this view is empty but {blob_count} blob file(s) exist on disk; \
             the view index was lost or wiped — run `basemind scan` to rebuild it"
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::helpers::{LIST_LIMIT_DEFAULT, LIST_LIMIT_MAX};

    #[test]
    fn list_limit_under_cap_is_not_clamped() {
        let (limit, clamped) = effective_list_limit(Some(100));
        assert_eq!(limit, 100);
        assert!(!clamped, "a request under the cap must not be flagged clamped");
    }

    #[test]
    fn list_limit_at_cap_is_not_clamped() {
        let (limit, clamped) = effective_list_limit(Some(LIST_LIMIT_MAX));
        assert_eq!(limit, LIST_LIMIT_MAX as usize);
        assert!(!clamped, "a request exactly at the cap is honored, not clamped");
    }

    #[test]
    fn list_limit_over_cap_is_clamped_and_signalled() {
        let (limit, clamped) = effective_list_limit(Some(LIST_LIMIT_MAX + 1));
        assert_eq!(limit, LIST_LIMIT_MAX as usize, "limit is clamped to the cap");
        assert!(clamped, "exceeding the cap must set the clamp flag (bug #17)");
    }

    #[test]
    fn list_limit_default_when_absent() {
        let (limit, clamped) = effective_list_limit(None);
        assert_eq!(limit, LIST_LIMIT_DEFAULT as usize);
        assert!(!clamped);
    }

    #[test]
    fn search_total_partial_when_cap_reached() {
        let limit = 10usize;
        let cap = search_max_total(limit);
        let matches_available = cap + 500;
        let mut total = 0usize;
        let mut partial = false;
        for _ in 0..matches_available {
            total += 1;
            if total >= cap {
                partial = true;
                break;
            }
        }
        assert_eq!(total, cap, "total saturates at the scan cap, not the true match count");
        assert!(partial, "hitting the cap must mark total as partial (bug #16)");
    }

    #[test]
    fn search_total_exact_when_under_cap() {
        let limit = 10usize;
        let cap = search_max_total(limit);
        let matches_available = 5usize;
        let mut total = 0usize;
        let mut partial = false;
        for _ in 0..matches_available {
            total += 1;
            if total >= cap {
                partial = true;
                break;
            }
        }
        assert_eq!(total, matches_available, "total is exact below the cap");
        assert!(!partial, "a query under the cap reports an exact, complete total");
    }

    #[test]
    fn status_note_absent_when_index_and_blobs_agree() {
        assert_eq!(blob_divergence_note(42, 100), None, "populated index: no note");
    }

    #[test]
    fn status_note_absent_for_unscanned_empty_repo() {
        assert_eq!(
            blob_divergence_note(0, 0),
            None,
            "empty index with no blobs is a legitimately unscanned repo, not a lost index"
        );
    }

    #[test]
    fn status_note_present_when_index_empty_but_blobs_exist() {
        let note = blob_divergence_note(0, 7);
        assert!(
            note.is_some(),
            "lost-index-over-live-blobs must surface a note (bug #10)"
        );
        let note = note.unwrap();
        assert!(note.contains("7 blob file"), "note reports the blob count");
        assert!(note.contains("scan"), "note suggests a rescan");
    }
}
