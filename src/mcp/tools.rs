//! `#[tool_router]` impl block for `GitmindServer`.
//!
//! Every `#[tool]`-annotated method below becomes a dispatchable MCP tool. Helpers live
//! in `super::helpers`; param/response shapes in `super::types`.

use std::collections::BTreeMap;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;

use super::GitmindServer;
use super::helpers::*;
use super::types::*;
use crate::query;

#[rmcp::tool_router(vis = "pub(super)")]
impl GitmindServer {
    /// File outline: symbols + imports (L1), optionally calls + docs (L2).
    #[tool(
        description = "Return the structural outline of a file: every symbol with name, kind, \
                       and start row/column, plus imports. Set `l2: true` to also include calls \
                       and doc comments (only returned if an L2 blob already exists for the \
                       file's current content)."
    )]
    async fn outline(
        &self,
        Parameters(params): Parameters<OutlineParams>,
    ) -> Result<CallToolResult, McpError> {
        let store = self.state.store.read().await;
        let l1 = query::file_outline(&store, &params.path).map_err(|e| {
            McpError::invalid_params(format!("file_outline({}): {e}", params.path), None)
        })?;

        let symbols = l1
            .symbols
            .iter()
            .map(|s| SymbolView {
                name: s.name.clone(),
                kind: kind_to_str(s.kind).to_string(),
                start_row: s.start_row,
                start_col: s.start_col,
                start_byte: s.start_byte,
                end_byte: s.end_byte,
                signature: s.signature.clone(),
            })
            .collect();
        let imports = l1
            .imports
            .iter()
            .map(|i| ImportView {
                module: i.module.clone(),
                raw: i.raw.clone(),
                start_byte: i.start_byte,
            })
            .collect();

        let mut response = OutlineResponse {
            path: params.path.clone(),
            language: l1.language.clone(),
            size_bytes: l1.size_bytes,
            had_errors: l1.had_errors,
            error_count: l1.error_count,
            symbols,
            imports,
            calls: None,
            docs: None,
            l2_status: None,
        };

        if params.l2 {
            let entry = store.lookup(&params.path).ok_or_else(|| {
                McpError::internal_error("file not indexed after outline succeeded", None)
            })?;
            match store.read_l2_by_hex(&entry.hash_hex) {
                Ok(Some(l2)) => {
                    response.calls = Some(
                        l2.calls
                            .iter()
                            .map(|c| CallView {
                                callee: c.callee.clone(),
                                start_byte: c.start_byte,
                            })
                            .collect(),
                    );
                    response.docs = Some(
                        l2.docs
                            .iter()
                            .map(|d| DocView {
                                text: d.text.clone(),
                                start_byte: d.start_byte,
                            })
                            .collect(),
                    );
                }
                Ok(None) => {
                    response.l2_status =
                        Some("missing — run `gitmind query outline <path> --l2` to materialize");
                }
                Err(e) => {
                    response.l2_status = Some("error");
                    return Err(McpError::internal_error(format!("read_l2: {e}"), None));
                }
            }
        }

        json_result(&response)
    }

    /// Substring search across symbol names, optionally filtered by kind.
    #[tool(
        description = "Search every indexed file for symbols whose name contains `needle`. \
                       Optional `kind` filter (function/struct/class/...). Returns up to `limit` \
                       (default 100, max 1000) results, each with path + line/column + signature."
    )]
    async fn search_symbols(
        &self,
        Parameters(params): Parameters<SearchSymbolsParams>,
    ) -> Result<CallToolResult, McpError> {
        let kind = params.kind.as_deref().map(parse_kind).transpose()?;
        let limit = params
            .limit
            .unwrap_or(SEARCH_LIMIT_DEFAULT)
            .min(SEARCH_LIMIT_MAX) as usize;

        let finder = memchr::memmem::Finder::new(params.needle.as_bytes());
        let max_total = limit.saturating_mul(64).max(2_000);
        let mut results: Vec<SearchHitView> = Vec::with_capacity(limit);
        let mut total: usize = 0;
        let mut total_is_partial = false;
        let cache = self.state.cache.load_full();
        'outer: for (path, l1) in &cache.by_path {
            for sym in &l1.symbols {
                if finder.find(sym.name.as_bytes()).is_none() {
                    continue;
                }
                if let Some(k) = kind
                    && sym.kind != k
                {
                    continue;
                }
                total += 1;
                if results.len() < limit {
                    results.push(SearchHitView {
                        path: path.clone(),
                        name: sym.name.clone(),
                        kind: kind_to_str(sym.kind).to_string(),
                        start_row: sym.start_row,
                        start_col: sym.start_col,
                        signature: sym.signature.clone(),
                    });
                }
                if total >= max_total {
                    total_is_partial = true;
                    break 'outer;
                }
            }
        }
        let truncated = total > limit || total_is_partial;
        json_result(&SearchResponse {
            total,
            truncated,
            results,
        })
    }

    /// List indexed files, optionally filtered by path substring and/or language.
    #[tool(
        description = "List indexed files with their language and size. Optional `path_contains` \
                       substring filter and `language` filter (rust/python/typescript/tsx/javascript/go). \
                       Default limit 200, max 5000."
    )]
    async fn list_files(
        &self,
        Parameters(params): Parameters<ListFilesParams>,
    ) -> Result<CallToolResult, McpError> {
        let limit = params
            .limit
            .unwrap_or(LIST_LIMIT_DEFAULT)
            .min(LIST_LIMIT_MAX) as usize;
        let store = self.state.store.read().await;

        let path_finder = params
            .path_contains
            .as_ref()
            .map(|n| memchr::memmem::Finder::new(n.as_bytes()));
        let lang_filter = params.language.as_deref();

        let mut files: Vec<ListFilesEntry> = Vec::with_capacity(limit.min(256));
        let mut total: usize = 0;
        for (p, e) in &store.index.files {
            let path_ok = path_finder
                .as_ref()
                .is_none_or(|f| f.find(p.as_bytes()).is_some());
            let lang_ok = lang_filter.is_none_or(|l| e.language == l);
            if !(path_ok && lang_ok) {
                continue;
            }
            total += 1;
            if files.len() < limit {
                files.push(ListFilesEntry {
                    path: p.clone(),
                    language: e.language.clone(),
                    size_bytes: e.size_bytes,
                });
            }
        }
        let truncated = total > limit;

        json_result(&ListFilesResponse {
            total,
            returned: files.len(),
            truncated,
            files,
        })
    }

    /// Heuristic reverse-dependency lookup via import statements.
    #[tool(
        description = "Return the list of indexed files whose imports mention `module`. \
                       Heuristic — matches by substring against the recorded module path of each import."
    )]
    async fn dependents(
        &self,
        Parameters(params): Parameters<DependentsParams>,
    ) -> Result<CallToolResult, McpError> {
        let paths: Vec<crate::path::RelPath> = crate::extract::l3::dependents_of(
            &params.module,
            &self.state.cache.load().imports_index,
        )
        .into_iter()
        .map(|p| crate::path::RelPath::from(p.as_path()))
        .collect();
        json_result(&DependentsResponse {
            module: params.module.clone(),
            paths,
        })
    }

    /// High-level repo + cache state.
    #[tool(
        description = "Quick report on the repo gitmind has indexed: file count, total bytes, \
                       per-language breakdown, root path, grammar cache directory, schema version."
    )]
    async fn status(
        &self,
        Parameters(_): Parameters<StatusParams>,
    ) -> Result<CallToolResult, McpError> {
        let store = self.state.store.read().await;
        let mut by_lang: BTreeMap<String, usize> = BTreeMap::new();
        let mut total_size: u64 = 0;
        for entry in store.index.files.values() {
            *by_lang.entry(entry.language.clone()).or_insert(0) += 1;
            total_size = total_size.saturating_add(entry.size_bytes);
        }
        let cache_dir = crate::lang::grammar_cache_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(unresolved)".to_string());
        let submodules = self
            .state
            .repo
            .as_ref()
            .map(|r| r.submodule_paths())
            .unwrap_or_default();
        json_result(&StatusResponse {
            file_count: store.index.files.len(),
            total_size_bytes: total_size,
            languages: by_lang,
            cache_dir,
            schema_version: crate::extract::SCHEMA_VER,
            root: self.state.root.display().to_string(),
            submodules,
        })
    }

    /// `git status --porcelain` shape for an agent.
    #[tool(
        description = "Return what's dirty in the working tree: staged adds/modifies/deletes, \
                       working-tree modifications, and untracked files. `is_clean: true` if all five \
                       buckets are empty. Requires `gitmind serve` to be run inside a git repository."
    )]
    async fn working_tree_status(
        &self,
        Parameters(_): Parameters<WorkingTreeStatusParams>,
    ) -> Result<CallToolResult, McpError> {
        let repo = require_git_repo(&self.state)?;
        let s = repo
            .status_porcelain()
            .map_err(|e| McpError::internal_error(format!("git status: {e}"), None))?;
        let is_clean = s.staged_added.is_empty()
            && s.staged_modified.is_empty()
            && s.staged_deleted.is_empty()
            && s.modified.is_empty()
            && s.untracked.is_empty();
        json_result(&WorkingTreeStatusView {
            staged_added: s.staged_added,
            staged_modified: s.staged_modified,
            staged_deleted: s.staged_deleted,
            modified: s.modified,
            untracked: s.untracked,
            is_clean,
        })
    }

    /// Walk HEAD ancestry and return the last N commits.
    #[tool(
        description = "Last N commits on the current branch, newest first. Each commit comes with \
                       sha, summary (first line of message), author, unix timestamp, and — when \
                       `include_files=true` (default) — the per-file change list relative to its first \
                       parent. Default 20 commits, max 100. Cached by HEAD sha."
    )]
    async fn recent_changes(
        &self,
        Parameters(params): Parameters<RecentChangesParams>,
    ) -> Result<CallToolResult, McpError> {
        let repo = require_git_repo(&self.state)?;
        let limit = params.limit.unwrap_or(LOG_LIMIT_DEFAULT).min(LOG_LIMIT_MAX);
        let head = head_sha(repo)?;
        let commits = self
            .state
            .git_cache
            .log(repo, &head, None, limit, params.include_files)
            .map_err(|e| McpError::internal_error(format!("log: {e}"), None))?;
        let view = commits
            .iter()
            .cloned()
            .map(|c| commit_to_view(c, params.include_files))
            .collect();
        let truncated = repo.is_shallow();
        json_result(&RecentChangesResponse {
            commits: view,
            truncated,
            truncated_reason: truncated.then_some("shallow_clone"),
        })
    }

    /// Filter the log to commits whose tree differs from the parent at `path`.
    #[tool(
        description = "Commits that modified `path`, newest first. Returns the same per-commit \
                       shape as `recent_changes` without the per-file list (the path is implicit). \
                       Default 20 commits, max 100."
    )]
    async fn commits_touching(
        &self,
        Parameters(params): Parameters<CommitsTouchingParams>,
    ) -> Result<CallToolResult, McpError> {
        let repo = require_git_repo(&self.state)?;
        let limit = params.limit.unwrap_or(LOG_LIMIT_DEFAULT).min(LOG_LIMIT_MAX);
        let head = head_sha(repo)?;
        let commits = self
            .state
            .git_cache
            .log(repo, &head, Some(&params.path), limit, false)
            .map_err(|e| McpError::internal_error(format!("log: {e}"), None))?;
        let view = commits
            .iter()
            .cloned()
            .map(|c| commit_to_view(c, false))
            .collect();
        let truncated = repo.is_shallow();
        json_result(&CommitsTouchingResponse {
            path: params.path,
            commits: view,
            truncated,
            truncated_reason: truncated.then_some("shallow_clone"),
        })
    }

    /// Symbol-level diff between the served view and another rev.
    #[tool(
        description = "Diff the symbol set of `path` between the current view and another revision \
                       (`rev`, defaults to HEAD). Returns three lists: `added` (in the current view, \
                       not at `rev`), `removed` (at `rev`, not in current view), and `common`. Useful \
                       for 'what symbols did this branch add' style questions without reading source."
    )]
    async fn diff_outline(
        &self,
        Parameters(params): Parameters<DiffOutlineParams>,
    ) -> Result<CallToolResult, McpError> {
        let repo = require_git_repo(&self.state)?;
        let rev_spec = params.rev.as_deref().unwrap_or("HEAD");
        let rev_sha = repo
            .resolve_rev(rev_spec)
            .map_err(|e| McpError::invalid_params(format!("resolve_rev({rev_spec}): {e}"), None))?;

        let cache = self.state.cache.load_full();
        let here = cache.by_path.get(&params.path).map(|l1| {
            l1.symbols
                .iter()
                .map(|s| (s.name.clone(), kind_to_str(s.kind)))
                .collect::<Vec<(String, &'static str)>>()
        });

        let rev_blob = repo.read_blob_at_rev(&rev_sha, &params.path).map_err(|e| {
            McpError::internal_error(format!("read blob {rev_sha}:{}: {e}", params.path), None)
        })?;

        let there: Option<Vec<(String, &'static str)>> = match rev_blob {
            Some(bytes) => {
                let lang =
                    crate::lang::detect(std::path::Path::new(&params.path)).ok_or_else(|| {
                        McpError::invalid_params(
                            format!("unsupported language for {}", params.path),
                            None,
                        )
                    })?;
                let l1 = crate::extract::l1::extract_l1(lang, &bytes).map_err(|e| {
                    McpError::internal_error(
                        format!("extract {rev_sha}:{}: {e}", params.path),
                        None,
                    )
                })?;
                Some(
                    l1.symbols
                        .into_iter()
                        .map(|s| (s.name, kind_to_str(s.kind)))
                        .collect(),
                )
            }
            None => None,
        };

        let (added, removed, common, note) = match (here, there) {
            (Some(h), Some(t)) => {
                let hs: ahash::AHashSet<(String, &'static str)> = h.iter().cloned().collect();
                let ts: ahash::AHashSet<(String, &'static str)> = t.iter().cloned().collect();
                let added = h
                    .iter()
                    .filter(|p| !ts.contains(*p))
                    .cloned()
                    .map(|(n, k)| DiffSymbolView {
                        name: n,
                        kind: k.to_string(),
                    })
                    .collect();
                let removed = t
                    .iter()
                    .filter(|p| !hs.contains(*p))
                    .cloned()
                    .map(|(n, k)| DiffSymbolView {
                        name: n,
                        kind: k.to_string(),
                    })
                    .collect();
                let common = h
                    .iter()
                    .filter(|p| ts.contains(*p))
                    .cloned()
                    .map(|(n, k)| DiffSymbolView {
                        name: n,
                        kind: k.to_string(),
                    })
                    .collect();
                (added, removed, common, None)
            }
            (Some(h), None) => (
                h.into_iter()
                    .map(|(n, k)| DiffSymbolView {
                        name: n,
                        kind: k.to_string(),
                    })
                    .collect(),
                Vec::new(),
                Vec::new(),
                Some(format!(
                    "path absent at {rev_spec}; entire file treated as added"
                )),
            ),
            (None, Some(t)) => (
                Vec::new(),
                t.into_iter()
                    .map(|(n, k)| DiffSymbolView {
                        name: n,
                        kind: k.to_string(),
                    })
                    .collect(),
                Vec::new(),
                Some(
                    "path not indexed in the current view; entire file treated as removed"
                        .to_string(),
                ),
            ),
            (None, None) => {
                return Err(McpError::invalid_params(
                    format!(
                        "path not present in current view or at {rev_spec}: {}",
                        params.path
                    ),
                    None,
                ));
            }
        };

        json_result(&DiffOutlineResponse {
            path: params.path,
            rev: rev_sha,
            added,
            removed,
            common,
            note,
        })
    }

    /// Cheap pickaxe: regex over changed file paths in HEAD ancestry.
    #[tool(
        description = "Walk recent commits (default last 200, max 1000) and return those whose \
                       changed-file list contains any path matching the regex `pattern`. Cheaper \
                       than `git log -G` because we only match paths, not patch text. Cached via \
                       the same `commit_files` cache as `recent_changes`."
    )]
    async fn find_commits_by_path(
        &self,
        Parameters(params): Parameters<FindCommitsByPathParams>,
    ) -> Result<CallToolResult, McpError> {
        let repo = require_git_repo(&self.state)?;
        let re = regex::Regex::new(&params.pattern)
            .map_err(|e| McpError::invalid_params(format!("invalid regex: {e}"), None))?;
        let window = params.window.unwrap_or(200).min(1000);
        let limit = params.limit.unwrap_or(50).min(500);

        let head = head_sha(repo)?;
        let commits = self
            .state
            .git_cache
            .log(repo, &head, None, window, true)
            .map_err(|e| McpError::internal_error(format!("log: {e}"), None))?;

        let mut hits: Vec<CommitView> = Vec::new();
        for c in commits.iter() {
            if hits.len() >= limit as usize {
                break;
            }
            if c.files.iter().any(|(p, _)| re.is_match(&p.to_str_lossy())) {
                hits.push(commit_to_view(c.clone(), true));
            }
        }
        json_result(&FindCommitsByPathResponse {
            pattern: params.pattern,
            window_inspected: window,
            commits: hits,
        })
    }

    /// Most-changed files in a recent commit window.
    #[tool(
        description = "Top-K files most-frequently modified in the last `window` commits on the \
                       current branch (default 200, max 2000). Each entry returns the per-kind \
                       breakdown (added/modified/deleted). Useful for a churn map of where the \
                       activity is concentrated."
    )]
    async fn hot_files(
        &self,
        Parameters(params): Parameters<HotFilesParams>,
    ) -> Result<CallToolResult, McpError> {
        let repo = require_git_repo(&self.state)?;
        let window = params.window.unwrap_or(200).min(2000);
        let top_k = params.top_k.unwrap_or(20).min(200) as usize;
        let head = head_sha(repo)?;
        let commits = self
            .state
            .git_cache
            .log(repo, &head, None, window, true)
            .map_err(|e| McpError::internal_error(format!("log: {e}"), None))?;

        let mut counts: ahash::AHashMap<crate::path::RelPath, (u32, u32, u32, u32)> =
            ahash::AHashMap::new();
        for c in commits.iter() {
            for (path, kind) in &c.files {
                let entry = counts.entry(path.clone()).or_insert((0, 0, 0, 0));
                entry.0 += 1;
                match kind {
                    crate::git::ChangeKind::Added => entry.1 += 1,
                    crate::git::ChangeKind::Modified | crate::git::ChangeKind::Renamed => {
                        entry.2 += 1
                    }
                    crate::git::ChangeKind::Deleted => entry.3 += 1,
                }
            }
        }
        let total_files_changed = counts.len() as u32;
        let mut ranked: Vec<HotFileEntry> = counts
            .into_iter()
            .map(|(path, (n, added, modified, deleted))| HotFileEntry {
                path,
                commits_touching: n,
                added,
                modified,
                deleted,
            })
            .collect();
        ranked.sort_by(|a, b| {
            b.commits_touching
                .cmp(&a.commits_touching)
                .then(a.path.cmp(&b.path))
        });
        ranked.truncate(top_k);

        json_result(&HotFilesResponse {
            window_inspected: window,
            total_files_changed,
            files: ranked,
        })
    }

    /// Content-level diff between two revs for one file.
    #[tool(
        description = "Hunks for `path` between `rev_old` and `rev_new`. Each hunk reports old/new \
                       1-based line ranges plus the changed text (lines prefixed with '-'/'+' for \
                       modifications). When the file is absent on one side, `present_at_old` or \
                       `present_at_new` indicates so and hunks describe the full add/remove."
    )]
    async fn diff_file(
        &self,
        Parameters(params): Parameters<DiffFileParams>,
    ) -> Result<CallToolResult, McpError> {
        let repo = require_git_repo(&self.state)?;
        let old_sha = repo.resolve_rev(&params.rev_old).map_err(|e| {
            McpError::invalid_params(format!("resolve_rev({}): {e}", params.rev_old), None)
        })?;
        let new_sha = repo.resolve_rev(&params.rev_new).map_err(|e| {
            McpError::invalid_params(format!("resolve_rev({}): {e}", params.rev_new), None)
        })?;
        let result = repo
            .diff_file(&old_sha, &new_sha, &params.path)
            .map_err(|e| McpError::internal_error(format!("diff: {e}"), None))?;
        let (hunks, present_old, present_new) = result.unwrap_or((Vec::new(), false, false));
        let hunks = hunks
            .into_iter()
            .map(|h| HunkView {
                kind: h.kind.as_str(),
                old_line_start: h.old_line_start,
                old_line_count: h.old_line_count,
                new_line_start: h.new_line_start,
                new_line_count: h.new_line_count,
                text: h.text,
            })
            .collect();
        json_result(&DiffFileResponse {
            path: params.path,
            rev_old: old_sha,
            rev_new: new_sha,
            present_at_old: present_old,
            present_at_new: present_new,
            hunks,
        })
    }

    /// Tree-sitter × git: commits where a specific symbol's body changed.
    #[tool(
        description = "List commits where the named symbol's body bytes changed (or where it was \
                       added/removed). Combines tree-sitter outline extraction with the commit log: \
                       a `recent_changes` filtered by symbol identity rather than file identity. \
                       Up to `limit` commits returned (default 20, max 100)."
    )]
    async fn symbol_history(
        &self,
        Parameters(params): Parameters<SymbolHistoryParams>,
    ) -> Result<CallToolResult, McpError> {
        let repo = require_git_repo(&self.state)?;
        let kind = params.kind.as_deref().map(parse_kind).transpose()?;
        let limit = params.limit.unwrap_or(20).min(100) as usize;
        let lang = crate::lang::detect(std::path::Path::new(&params.path)).ok_or_else(|| {
            McpError::invalid_params(format!("unsupported language: {}", params.path), None)
        })?;
        let hash_mode = match params.hash_mode.as_deref() {
            Some(s) => parse_hash_mode(s)?,
            None => HashMode::Normalized,
        };

        let head = head_sha(repo)?;
        let commits = self
            .state
            .git_cache
            .log(repo, &head, Some(&params.path), limit as u32 * 4, false)
            .map_err(|e| McpError::internal_error(format!("log: {e}"), None))?;

        // Walk commits oldest→newest so "introduced" / "modified" labels are correct.
        let chronological: Vec<crate::git::CommitInfo> = commits.iter().cloned().rev().collect();

        let mut history = Vec::new();
        let mut prev_fp: Option<Vec<u8>> = None;
        let mut prev_existed = false;
        let mut inspected: u32 = 0;
        for c in chronological {
            inspected += 1;
            let blob = repo
                .read_blob_at_rev_with_oid(&c.sha, &params.path)
                .map_err(|e| McpError::internal_error(format!("blob: {e}"), None))?;
            // Look up (or insert) the parsed outline + source for this blob OID. Cache hits
            // skip both the blob copy and the tree-sitter parse — symbol_history walks 20+
            // commits and many of them share blob OIDs, so this is the dominant speedup.
            let fingerprint = match blob {
                Some((bytes, oid)) => {
                    outline_entry_for_blob(&self.state.outline_cache, oid, lang, bytes).and_then(
                        |entry| symbol_fingerprint(&entry, &params.name, kind, lang, hash_mode),
                    )
                }
                None => None,
            };
            let change = match (prev_existed, fingerprint.as_ref()) {
                (false, Some(_)) => Some("introduced"),
                (true, None) => Some("removed"),
                (true, Some(curr)) => {
                    if prev_fp.as_deref() != Some(curr.as_slice()) {
                        Some("modified")
                    } else {
                        None
                    }
                }
                (false, None) => None,
            };
            if let Some(kind_str) = change {
                history.push(SymbolHistoryEntry {
                    sha: c.sha.clone(),
                    short_sha: c.short_sha.clone(),
                    summary: c.summary.clone(),
                    author: c.author.clone(),
                    author_time_unix: c.author_time_unix,
                    change: kind_str,
                });
            }
            prev_existed = fingerprint.is_some();
            prev_fp = fingerprint;
        }
        history.reverse();
        history.truncate(limit);

        let truncated = repo.is_shallow();
        json_result(&SymbolHistoryResponse {
            path: params.path,
            name: params.name,
            kind: kind.map(|k| kind_to_str(k).to_string()),
            commits_inspected: inspected,
            history,
            hash_mode: hash_mode.as_str(),
            truncated,
            truncated_reason: truncated.then_some("shallow_clone"),
        })
    }

    /// Line-level blame, optionally clamped to a 1-based inclusive line range.
    #[tool(
        description = "Blame the file at the given revision (default HEAD), returning one hunk per \
                       consecutive run of lines that share a source commit. Optional 1-based \
                       `line_start`/`line_end` clamp to a range. Each hunk carries commit sha, \
                       author, unix time, summary, and the renamed source path if applicable. \
                       Results are cached forever by (suspect_sha, path, range)."
    )]
    async fn blame_file(
        &self,
        Parameters(params): Parameters<BlameFileParams>,
    ) -> Result<CallToolResult, McpError> {
        let repo = require_git_repo(&self.state)?;
        let suspect_sha = match params.rev.as_deref() {
            Some(r) => repo
                .resolve_rev(r)
                .map_err(|e| McpError::invalid_params(format!("resolve_rev({r}): {e}"), None))?,
            None => head_sha(repo)?,
        };
        let range = match (params.line_start, params.line_end) {
            (Some(lo), Some(hi)) => Some((lo, hi)),
            (None, None) => None,
            _ => {
                return Err(McpError::invalid_params(
                    "line_start and line_end must be provided together",
                    None,
                ));
            }
        };
        let result = match self
            .state
            .git_cache
            .blame(repo, &suspect_sha, &params.path, range)
        {
            Ok(r) => r,
            Err(e) => {
                // Size-cap rejections are not server errors — return an empty truncated
                // response so the agent can fall back to a smaller line range or skip.
                if let Some(too_large) = blame_too_large_response(&params.path, &suspect_sha, &e) {
                    return json_result(&too_large);
                }
                return Err(McpError::internal_error(format!("blame: {e}"), None));
            }
        };
        let hunks = result.hunks.iter().map(blame_hunk_view).collect();
        // Even when blame succeeded, a shallow clone may have silently truncated the
        // history walk — surface the flag so the agent treats hunks attributed to the
        // shallow boundary's pseudo-root commit appropriately.
        let truncated_reason: Option<&'static str> = match result.truncated_reason.as_deref() {
            Some("shallow_clone") => Some("shallow_clone"),
            Some(_) => Some("truncated"),
            None if repo.is_shallow() => Some("shallow_clone"),
            None => None,
        };
        json_result(&BlameResponse {
            path: result.path.clone(),
            suspect_sha: result.suspect_sha.clone(),
            hunks,
            truncated: truncated_reason.is_some(),
            truncated_reason,
        })
    }

    /// Blame clamped to a specific tree-sitter symbol.
    #[tool(
        description = "Blame only the lines that belong to a named symbol in a file. Resolves the \
                       symbol via the cached L1 outline (must be indexed in the current view) and \
                       feeds its line range to `blame_file`. `kind` disambiguates same-named \
                       symbols of different kinds."
    )]
    async fn blame_symbol(
        &self,
        Parameters(params): Parameters<BlameSymbolParams>,
    ) -> Result<CallToolResult, McpError> {
        let repo = require_git_repo(&self.state)?;
        let kind = params.kind.as_deref().map(parse_kind).transpose()?;
        let cache = self.state.cache.load_full();
        let l1 = cache.by_path.get(&params.path).ok_or_else(|| {
            McpError::invalid_params(
                format!("file not indexed in current view: {}", params.path),
                None,
            )
        })?;
        let sym = l1
            .symbols
            .iter()
            .find(|s| s.name == params.name && kind.is_none_or(|k| s.kind == k))
            .ok_or_else(|| {
                McpError::invalid_params(
                    format!(
                        "symbol `{}`{} not found in {}",
                        params.name,
                        kind.map(|k| format!(" (kind={})", kind_to_str(k)))
                            .unwrap_or_default(),
                        params.path
                    ),
                    None,
                )
            })?;
        let (line_start, line_end) = symbol_line_range(repo, &params.path, sym);
        let suspect_sha = match params.rev.as_deref() {
            Some(r) => repo
                .resolve_rev(r)
                .map_err(|e| McpError::invalid_params(format!("resolve_rev({r}): {e}"), None))?,
            None => head_sha(repo)?,
        };
        let result = match self.state.git_cache.blame(
            repo,
            &suspect_sha,
            &params.path,
            Some((line_start, line_end)),
        ) {
            Ok(r) => r,
            Err(e) => {
                if let Some(too_large) = blame_symbol_too_large_response(
                    &params.path,
                    &suspect_sha,
                    sym,
                    line_start,
                    line_end,
                    &e,
                ) {
                    return json_result(&too_large);
                }
                return Err(McpError::internal_error(format!("blame: {e}"), None));
            }
        };
        let hunks = result.hunks.iter().map(blame_hunk_view).collect();
        let truncated_reason: Option<&'static str> = match result.truncated_reason.as_deref() {
            Some("shallow_clone") => Some("shallow_clone"),
            Some(_) => Some("truncated"),
            None if repo.is_shallow() => Some("shallow_clone"),
            None => None,
        };
        json_result(&BlameSymbolResponse {
            path: result.path.clone(),
            suspect_sha: result.suspect_sha.clone(),
            name: sym.name.clone(),
            kind: kind_to_str(sym.kind).to_string(),
            line_start,
            line_end,
            hunks,
            truncated: truncated_reason.is_some(),
            truncated_reason,
        })
    }

    /// Incoming call sites for any callee whose identifier matches `name`.
    #[tool(
        description = "List call sites of any function/method whose callee identifier matches \
                       `name`. Backed by the Fjall inverted index over L2 call captures — \
                       returns hits as (path, line, column, exact callee). Best-effort: no \
                       scope-aware resolution, so `Foo::bar()` and `bar()` both match \
                       name=\"bar\". Returns up to `limit` results (default 100, max 1000). \
                       Requires the index to have been populated by a scan with `eager_l2=true` \
                       (the default); returns an empty hit list otherwise."
    )]
    async fn find_references(
        &self,
        Parameters(params): Parameters<FindReferencesParams>,
    ) -> Result<CallToolResult, McpError> {
        let store = self.state.store.read().await;
        let idx = store.index_db.as_ref().cloned();
        drop(store);
        run_find_references(idx.as_ref(), params)
    }

    /// Callers of a specific definition (path + name + optional kind).
    #[tool(
        description = "Given a definition (path + name + optional kind), list every call site \
                       whose callee identifier matches. Resolves the definition via the symbols \
                       index first (echoed back in `definition`), then does the same name-based \
                       scan as `find_references`. Useful when you need to anchor the search on a \
                       specific symbol rather than a bare name. Same scope-resolution caveat \
                       applies. Default limit 100, max 1000."
    )]
    async fn find_callers(
        &self,
        Parameters(params): Parameters<FindCallersParams>,
    ) -> Result<CallToolResult, McpError> {
        let store = self.state.store.read().await;
        let idx = store.index_db.as_ref().cloned();
        drop(store);
        let cache = self.state.cache.load_full();
        run_find_callers(idx.as_ref(), params, &cache)
    }

    /// Workdir + branch + HEAD sha.
    #[tool(
        description = "Repository identity: workdir path, current branch name (if HEAD is on one), \
                       full HEAD sha, short HEAD sha. Pairs well with `working_tree_status`."
    )]
    async fn repo_info(
        &self,
        Parameters(_): Parameters<RepoInfoParams>,
    ) -> Result<CallToolResult, McpError> {
        let repo = require_git_repo(&self.state)?;
        let info = repo
            .info()
            .map_err(|e| McpError::internal_error(format!("repo info: {e}"), None))?;
        json_result(&RepoInfoResponse {
            workdir: info.workdir.display().to_string(),
            head_sha: info.head_sha,
            head_short_sha: info.head_short_sha,
            branch: info.branch,
        })
    }
}
