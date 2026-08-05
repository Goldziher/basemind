//! The `git` domain tool shim for `BasemindServer`.
//!
//! One tool, one required `mode` — working-tree status, commit log, churn, diffs, blame, and
//! symbol history — dispatched to `helpers_git::run_git`. Thin wrapper: every body lives in
//! `helpers_git.rs` and `helpers_git_file.rs`.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;
use serde_json::Value;

use super::BasemindServer;
use super::helpers::record_call;
use super::lenient::Lenient;
use super::mode::GitMode;
use super::types::{BlameSymbolParams, DiffFileParams, RecentChangesParams};
use super::types_git::GitParams;

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_git")]
impl BasemindServer {
    // No `output_schema`: the eleven modes return eleven different response shapes, and SEP-2106
    // allows exactly one per tool. Declaring a union would mean nested structs, which schemars
    // emits as `$ref` into `$defs` — the construct that silently dropped the whole registry in
    // GH #50. The per-mode shapes are documented in the description instead. ~keep
    #[tool(
        description = "Read this repository's git history without shelling out to `git log`, \
        `git blame`, `git diff` or `git status`. `mode` is required. `status` is `git status` for \
        an agent: staged adds/modifies/deletes, working-tree modifications, untracked files, and \
        `is_clean` when all five buckets are empty — ask it before assuming the tree is dirty or \
        clean. `recent` is 'what changed recently': the last N commits on the current branch, \
        newest first, each with sha, summary, author, unix time and (unless `include_files=false`) \
        the per-file change list. It is a bounded RECENCY WINDOW, not a search — `limit` is page \
        size (default 20, max 100), so it sees at most the newest ~100 commits. `search` is the \
        one for 'what did <author> do', 'which commit mentions X', or '<author>'s last commit': \
        tokenized AND full-text over the ENTIRE branch history — author name + email, commit \
        summary, and full body — so it finds matches arbitrarily far back, newest first. `field` \
        scopes it to `author`, `message`, or `all` (default; `summary`/`body` alias `message`); \
        served from the git-history index when fresh, otherwise a bounded live fallback flagged \
        `partial` (author + summary only). `touching` answers 'when was this file changed' — the \
        commits that modified one `path`, newest first. `by_path` is a cheap pickaxe: commits in a \
        recent `window` (default 200, max 1000) whose changed-file list has a path matching the \
        regex `pattern`; it matches PATHS ONLY, never patch text. `churn` answers 'what changes \
        most often' / 'where is the hot code': the `top_k` (default 20, max 200) most-frequently \
        modified files in the last `window` commits (default 200, max 2000), with an \
        added/modified/deleted breakdown. `diff` is `git diff <rev_old> <rev_new> -- <path>`: \
        hunks with 1-based old/new line ranges and '-'/'+' prefixed text, plus \
        `present_at_old`/`present_at_new` when the file exists on only one side. `diff_outline` is \
        the structural version — which SYMBOLS the current view added, removed, or kept versus \
        `rev` (default HEAD) — use it to see what a branch introduced without reading the source. \
        `blame` answers 'who last touched this line' / 'who wrote this': one hunk per consecutive \
        run of lines sharing a source commit, at `rev` (default HEAD), optionally clamped to \
        `line_start`..`line_end` (1-based, inclusive, supplied together). `blame_symbol` does the \
        same for one named symbol — it resolves the symbol through the indexed outline (so the \
        file must be in the current view), then blames exactly its line span; `kind` disambiguates \
        same-named symbols. `symbol_history` answers 'when did this function actually change': the \
        commits where the symbol's BODY fingerprint changed, or where it was introduced or \
        removed — file-level history filtered down to symbol identity via tree-sitter; `hash_mode` \
        picks `normalized` (default), `structural`, or `structural_loose`. Paths are \
        repository-relative and forward-slash. Every paged mode returns `next_cursor`; cursors are \
        scoped to HEAD and a moved HEAD comes back as `cursor_invalidated`. A shallow clone sets \
        `truncated` / `truncated_reason`, so a missing commit is inconclusive rather than absent. \
        Requires a git repository. Parameters that belong to another mode are rejected, not \
        ignored.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn git(
        &self,
        Parameters(Lenient(p)): Parameters<Lenient<GitParams>>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __key = p.mode.telemetry_key();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = super::helpers_git::run_git(&self.state, p).await;
        record_call(&self.state, __key, &__params_json, __started, &__result);
        __result
    }
}

/// Named in-process entry points for the three git operations the `basemind-agent` engine exposes
/// as its own tools, bridged through [`super::agent_api`].
///
/// They are deliberately NOT `#[tool]`s: the MCP registry advertises exactly one `git` tool. Each
/// builds the same [`GitParams`] an MCP caller would send, so telemetry, validation, and the
/// response shape are identical to `git { mode: … }`.
impl BasemindServer {
    pub(crate) async fn recent_changes(
        &self,
        Parameters(p): Parameters<RecentChangesParams>,
    ) -> Result<CallToolResult, McpError> {
        self.git(Parameters(Lenient(GitParams {
            limit: p.limit,
            include_files: Some(p.include_files),
            cursor: p.cursor,
            ..GitParams::new(GitMode::Recent)
        })))
        .await
    }

    pub(crate) async fn blame_symbol(
        &self,
        Parameters(p): Parameters<BlameSymbolParams>,
    ) -> Result<CallToolResult, McpError> {
        self.git(Parameters(Lenient(GitParams {
            path: Some(p.path),
            name: Some(p.name),
            kind: p.kind,
            rev: p.rev,
            limit: p.limit,
            cursor: p.cursor,
            ..GitParams::new(GitMode::BlameSymbol)
        })))
        .await
    }

    pub(crate) async fn diff_file(
        &self,
        Parameters(p): Parameters<DiffFileParams>,
    ) -> Result<CallToolResult, McpError> {
        self.git(Parameters(Lenient(GitParams {
            path: Some(p.path),
            rev_old: Some(p.rev_old),
            rev_new: Some(p.rev_new),
            ..GitParams::new(GitMode::Diff)
        })))
        .await
    }
}
