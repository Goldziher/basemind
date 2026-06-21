//! `#[tool_router]` impl block for `BasemindServer`.
//!
//! Every `#[tool]`-annotated method below becomes a dispatchable MCP tool. Helpers live
//! in `super::helpers`; param/response shapes in `super::types`.

use std::collections::BTreeMap;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;
use serde_json::Value;

use super::BasemindServer;
use super::helpers::*;
use super::lenient::Lenient;
use super::types::*;
use crate::query;

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_core")]
impl BasemindServer {
    /// File outline: symbols + imports (L1), optionally calls + docs (L2).
    #[tool(
        description = "Return the structural outline of a file: every symbol with name, kind, \
                       and start row/column, plus imports. Set `l2: true` to also include calls \
                       and doc comments (only returned if an L2 blob already exists for the \
                       file's current content). Optional `max_tokens` bounds the returned \
                       `symbols` list (not imports/calls/docs); when it drops symbols the \
                       response sets `budgeted: true`."
    )]
    pub(crate) async fn outline(
        &self,
        Parameters(Lenient(params)): Parameters<Lenient<OutlineParams>>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&params).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            // Helper: map an L1 blob to its (symbols, imports) view fields.
            fn l1_views(l1: &crate::extract::FileMapL1) -> (Vec<SymbolView>, Vec<ImportView>) {
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
                (symbols, imports)
            }

            let mut response = if params.l2 {
                // L2 path: take the store lock once for both the L1 read and the L2 read.
                let store = self.state.store.read().await;
                let l1 = query::file_outline(&store, &params.path).map_err(|e| {
                    McpError::invalid_params(format!("file_outline({}): {e}", params.path), None)
                })?;
                let (symbols, imports) = l1_views(&l1);
                let mut r = OutlineResponse {
                    path: params.path.clone(),
                    language: l1.language.clone(),
                    size_bytes: l1.size_bytes,
                    had_errors: l1.had_errors,
                    error_count: l1.error_count,
                    budgeted: false,
                    symbols,
                    imports,
                    calls: None,
                    docs: None,
                    l2_status: None,
                };
                let entry = store.lookup(&params.path).ok_or_else(|| {
                    McpError::internal_error("file not indexed after outline succeeded", None)
                })?;
                match store.read_l2_by_hex(&entry.hash_hex) {
                    Ok(Some(l2)) => {
                        r.calls = Some(
                            l2.calls
                                .iter()
                                .map(|c| CallView {
                                    callee: c.callee.clone(),
                                    start_byte: c.start_byte,
                                })
                                .collect(),
                        );
                        r.docs = Some(
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
                        r.l2_status = Some(
                            "missing — run `basemind query outline <path> --l2` to materialize",
                        );
                    }
                    Err(e) => {
                        r.l2_status = Some("error");
                        return Err(McpError::internal_error(format!("read_l2: {e}"), None));
                    }
                }
                r
            } else {
                // L1-only path: serve from the in-RAM MapCache — no store lock, no disk
                // read. The cache is authoritative (rebuilt on every rescan). Fall back
                // to the store only on a cache miss (file indexed but blob evicted, which
                // should not happen in normal operation).
                let cache = self.state.cache.load();
                if let Some(l1) = cache.by_path.get(&params.path) {
                    let (symbols, imports) = l1_views(l1);
                    OutlineResponse {
                        path: params.path.clone(),
                        language: l1.language.clone(),
                        size_bytes: l1.size_bytes,
                        had_errors: l1.had_errors,
                        error_count: l1.error_count,
                        budgeted: false,
                        symbols,
                        imports,
                        calls: None,
                        docs: None,
                        l2_status: None,
                    }
                } else {
                    // Cache miss fallback.
                    let store = self.state.store.read().await;
                    let l1 = query::file_outline(&store, &params.path).map_err(|e| {
                        McpError::invalid_params(
                            format!("file_outline({}): {e}", params.path),
                            None,
                        )
                    })?;
                    let (symbols, imports) = l1_views(&l1);
                    OutlineResponse {
                        path: params.path.clone(),
                        language: l1.language.clone(),
                        size_bytes: l1.size_bytes,
                        had_errors: l1.had_errors,
                        error_count: l1.error_count,
                        budgeted: false,
                        symbols,
                        imports,
                        calls: None,
                        docs: None,
                        l2_status: None,
                    }
                }
            };

            // Token budget bounds the symbols list (the high-volume part of an outline);
            // imports / calls / docs are left intact. Applied before serializing.
            if params.max_tokens.is_some() {
                let budgeted = super::budget::apply_budget(
                    std::mem::take(&mut response.symbols),
                    params.max_tokens,
                );
                response.symbols = budgeted.items;
                response.budgeted = budgeted.budgeted;
            }
            json_result(&response)
        }
        .await;
        record_call(&self.state, "outline", &__params_json, __started, &__result);
        __result
    }

    /// Substring search across symbol names, optionally filtered by kind.
    #[tool(
        description = "Search every indexed file for symbols whose name contains the `needle` \
                       argument (substring, case-sensitive). \
                       Optional `kind` filter (function/struct/class/...). Returns up to `limit` \
                       (default 100, max 1000) results, each with path + line/column + signature. \
                       Pass `cursor` from a previous response to fetch the next page; absent \
                       means no more results. Cursors invalidate on rescan — caller must \
                       restart when `cursor_invalidated` is set. Optional `max_tokens` bounds \
                       the response: results are kept best-first until the budget is hit, then \
                       `budgeted: true` + a `next_cursor` signal the dropped tail is pageable."
    )]
    pub(crate) async fn search_symbols(
        &self,
        Parameters(Lenient(params)): Parameters<Lenient<SearchSymbolsParams>>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&params).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            use std::sync::atomic::Ordering;

            let kind = params.kind.as_deref().map(parse_kind).transpose()?;
            let limit = params
                .limit
                .unwrap_or(SEARCH_LIMIT_DEFAULT)
                .min(SEARCH_LIMIT_MAX) as usize;
            let generation = self.state.cache_generation.load(Ordering::Relaxed);

            // Decode cursor and check snapshot id. Stale cursor → bail with empty page +
            // cursor_invalidated=true so the caller can restart.
            let skip = match params.cursor.as_ref() {
                Some(c) => {
                    let (offset, snapshot_id) = c.decode_in_memory()?;
                    if snapshot_id != generation {
                        return json_result(&SearchResponse {
                            total: 0,
                            truncated: false,
                            budgeted: false,
                            results: Vec::new(),
                            next_cursor: None,
                            cursor_invalidated: true,
                        });
                    }
                    offset as usize
                }
                None => 0,
            };

            // Empty needle matches every symbol — never what the caller wants and
            // expensive on large repos. Return immediately.
            if params.needle.is_empty() {
                return json_result(&SearchResponse {
                    total: 0,
                    truncated: false,
                    budgeted: false,
                    results: Vec::new(),
                    next_cursor: None,
                    cursor_invalidated: false,
                });
            }
            let finder = memchr::memmem::Finder::new(params.needle.as_bytes());
            let max_total = limit.saturating_mul(64).max(2_000);
            let mut results: Vec<SearchHitView> = Vec::with_capacity(limit);
            let mut total: usize = 0;
            // `seen` tracks how many *matching* entries we've walked past, including the
            // first `skip` we discard. The `total` counter only includes the entries that
            // make it into / past this page.
            let mut seen: usize = 0;
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
                    if seen < skip {
                        seen += 1;
                        continue;
                    }
                    seen += 1;
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
            // Apply the token budget AFTER the limit page is built but BEFORE computing the
            // cursor, so the cursor advances by the *kept* count — the next page resumes
            // exactly at the first dropped item with no gap or overlap.
            let budget = super::budget::apply_budget(results, params.max_tokens);
            let results = budget.items;
            let budgeted = budget.budgeted;
            // `next_cursor` advances by the kept page size (results.len()) past the skip
            // offset. More remains when the scan saw more than we kept (limit cap) OR the
            // budget dropped items from this page.
            let next_cursor = if total > results.len() {
                Some(super::cursor::Cursor::encode_in_memory(
                    (skip + results.len()) as u64,
                    generation,
                ))
            } else {
                None
            };
            json_result(&SearchResponse {
                total,
                truncated,
                budgeted,
                results,
                next_cursor,
                cursor_invalidated: false,
            })
        }
        .await;
        record_call(
            &self.state,
            "search_symbols",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    /// List indexed files, optionally filtered by path substring and/or language.
    #[tool(
        description = "List indexed files with their language and size. Optional `path_contains` \
                       substring filter and `language` filter (rust/python/typescript/tsx/javascript/go). \
                       Default limit 200, max 5000. Pass `cursor` from a previous response to \
                       fetch the next page; absent means no more results. Cursors invalidate on \
                       rescan — caller must restart when `cursor_invalidated` is set. Optional \
                       `max_tokens` bounds the response: files are kept in order until the \
                       budget is hit, then `budgeted: true` + a `next_cursor` page the rest."
    )]
    pub(crate) async fn list_files(
        &self,
        Parameters(params): Parameters<ListFilesParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&params).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            use std::sync::atomic::Ordering;

            let limit = params
                .limit
                .unwrap_or(LIST_LIMIT_DEFAULT)
                .min(LIST_LIMIT_MAX) as usize;
            let generation = self.state.cache_generation.load(Ordering::Relaxed);

            // List uses the underlying `store.index.files` BTreeMap which is also rebuilt
            // on rescan — treat the same `cache_generation` as the snapshot id, since
            // `cache.store` always happens after a store mutation.
            let skip = match params.cursor.as_ref() {
                Some(c) => {
                    let (offset, snapshot_id) = c.decode_in_memory()?;
                    if snapshot_id != generation {
                        return json_result(&ListFilesResponse {
                            total: 0,
                            returned: 0,
                            truncated: false,
                            budgeted: false,
                            files: Vec::new(),
                            next_cursor: None,
                            cursor_invalidated: true,
                        });
                    }
                    offset as usize
                }
                None => 0,
            };
            let store = self.state.store.read().await;

            let path_finder = params
                .path_contains
                .as_ref()
                .map(|n| memchr::memmem::Finder::new(n.as_bytes()));
            let lang_filter = params.language.as_deref();

            let mut files: Vec<ListFilesEntry> = Vec::with_capacity(limit.min(256));
            let mut total: usize = 0;
            let mut seen: usize = 0;
            for (p, e) in &store.index.files {
                let path_ok = path_finder
                    .as_ref()
                    .is_none_or(|f| f.find(p.as_bytes()).is_some());
                let lang_ok = lang_filter.is_none_or(|l| e.language == l);
                if !(path_ok && lang_ok) {
                    continue;
                }
                if seen < skip {
                    seen += 1;
                    continue;
                }
                seen += 1;
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
            // Budget the file list before computing the cursor so the next page resumes at
            // the first dropped entry (cursor advances by the kept count, not the scanned count).
            let budget = super::budget::apply_budget(files, params.max_tokens);
            let files = budget.items;
            let budgeted = budget.budgeted;
            let next_cursor = if total > files.len() {
                Some(super::cursor::Cursor::encode_in_memory(
                    (skip + files.len()) as u64,
                    generation,
                ))
            } else {
                None
            };

            json_result(&ListFilesResponse {
                total,
                returned: files.len(),
                truncated,
                budgeted,
                files,
                next_cursor,
                cursor_invalidated: false,
            })
        }
        .await;
        record_call(
            &self.state,
            "list_files",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    /// Heuristic reverse-dependency lookup via import statements.
    #[tool(
        description = "Return the list of indexed files whose imports mention the `module` argument. \
                       Heuristic — matches by substring against the recorded module path of each import."
    )]
    pub(crate) async fn dependents(
        &self,
        Parameters(Lenient(params)): Parameters<Lenient<DependentsParams>>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&params).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
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
        .await;
        record_call(
            &self.state,
            "dependents",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    /// High-level repo + cache state.
    #[tool(
        description = "Quick report on the repo basemind has indexed: file count, total bytes, \
                       per-language breakdown, root path, grammar cache directory, schema version."
    )]
    pub(crate) async fn status(
        &self,
        Parameters(_): Parameters<StatusParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = Value::Null;
        let __result: Result<CallToolResult, McpError> = async {
            let store = self.state.store.read().await;
            // Count into a borrowed-key map to avoid one String::clone() per file.
            // The store lock is held for the entire loop, so &str borrows into the
            // store are valid. Convert to BTreeMap<String,usize> once at the end —
            // cost is O(distinct languages), not O(total files).
            let mut by_lang_ref: BTreeMap<&str, usize> = BTreeMap::new();
            let mut total_size: u64 = 0;
            for entry in store.index.files.values() {
                *by_lang_ref.entry(entry.language.as_str()).or_insert(0) += 1;
                total_size = total_size.saturating_add(entry.size_bytes);
            }
            let by_lang: BTreeMap<String, usize> = by_lang_ref
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect();
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
        .await;
        record_call(&self.state, "status", &__params_json, __started, &__result);
        __result
    }

    /// Incoming call sites for any callee whose identifier contains `name`.
    #[tool(
        description = "List call sites of any function/method whose callee identifier contains \
                       the `name` argument (case-sensitive substring match). Backed by the Fjall inverted \
                       index over L2 call captures — returns hits as (path, line, column, exact \
                       callee). No scope-aware resolution: `Foo::bar()` and `bar()` both match \
                       name=\"bar\". Returns up to `limit` results (default 100, max 1000); \
                       scan is bounded by `scan_cap = limit * 8` matching entries. Requires the \
                       index to have been populated by a scan with `eager_l2=true` (the default). \
                       Pass `cursor` from a previous response to fetch the next page; absent \
                       means no more results. Optional `max_tokens` bounds the response: hits \
                       are kept best-first until the budget is hit, then `budgeted: true` + a \
                       `next_cursor` page the dropped tail."
    )]
    pub(crate) async fn find_references(
        &self,
        Parameters(Lenient(params)): Parameters<Lenient<FindReferencesParams>>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&params).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            let store = self.state.store.read().await;
            let idx = store.index_db.as_ref().cloned();
            drop(store);
            run_find_references(idx.as_ref(), params)
        }
        .await;
        record_call(
            &self.state,
            "find_references",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    /// Callers of a specific definition (path + name + optional kind).
    #[tool(
        description = "Given a definition (`path` + `name` + optional kind), list every call site \
                       whose callee identifier matches. Resolves the definition via the symbols \
                       index first (echoed back in `definition`), then does the same name-based \
                       scan as `find_references`. Useful when you need to anchor the search on a \
                       specific symbol rather than a bare name. Same scope-resolution caveat \
                       applies. Default limit 100, max 1000. Pass `cursor` from a previous \
                       response to fetch the next page; absent means no more results. Optional \
                       `max_tokens` bounds the response: hits are kept best-first until the \
                       budget is hit, then `budgeted: true` + a `next_cursor` page the rest."
    )]
    pub(crate) async fn find_callers(
        &self,
        Parameters(Lenient(params)): Parameters<Lenient<FindCallersParams>>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&params).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            let store = self.state.store.read().await;
            let idx = store.index_db.as_ref().cloned();
            drop(store);
            let cache = self.state.cache.load_full();
            run_find_callers(idx.as_ref(), params, &cache)
        }
        .await;
        record_call(
            &self.state,
            "find_callers",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    /// Regex content search across indexed files.
    #[tool(
        description = "Regex search across indexed files: the `pattern` argument is Rust regex syntax. Returns line + \
                       column + matched text plus optional 1-line context. Prefer \
                       `search_symbols` when the pattern is a plain substring identifier — \
                       that's index-backed and faster. Bounded by `scan_cap = limit * 8` files; \
                       pass `language` or `path_contains` to narrow the scan. Default limit 100, \
                       max 1000. Pass `cursor` from a previous response to fetch the next page; \
                       absent means no more results. Cursors invalidate on rescan. Optional \
                       `max_tokens` bounds the response: hits are kept best-first until the \
                       budget is hit, then `budgeted: true` + a `next_cursor` page the rest \
                       (the boundary file may re-appear on the next page)."
    )]
    pub(crate) async fn workspace_grep(
        &self,
        Parameters(Lenient(params)): Parameters<Lenient<WorkspaceGrepParams>>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&params).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> =
            async { run_workspace_grep(&self.state, params) }.await;
        record_call(
            &self.state,
            "workspace_grep",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    /// Types / classes that implement, extend, or inherit from a name containing `trait_name`.
    #[tool(
        description = "Find types that implement, extend, or inherit from the `trait_name` argument \
                       (a trait / interface / base class). Returns each (trait, implementor, file, line, column) pair. \
                       Matching: `trait_name` is a case-sensitive substring match against captured \
                       identifiers (full-partition scan). Covers Rust (`impl Trait for Type`), \
                       Python (`class Foo(Bar):`), TypeScript / TSX (`class X extends Y`, \
                       `class X implements Y`, `interface X extends Y`), and JavaScript \
                       (`class X extends Y`). Go interface satisfaction is structural and not \
                       detected. Bounded by `scan_cap = limit * 8` — pass `cursor` from a \
                       previous response to fetch the next page; cursors remain stable across \
                       rescans (Fjall-backed). Optional `max_tokens` bounds the response: hits \
                       are kept best-first until the budget is hit, then `budgeted: true` + a \
                       `next_cursor` page the rest."
    )]
    pub(crate) async fn find_implementations(
        &self,
        Parameters(Lenient(params)): Parameters<Lenient<FindImplementationsParams>>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&params).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            let store = self.state.store.read().await;
            let idx = store.index_db.as_ref().cloned();
            drop(store);
            let cache = self.state.cache.load_full();
            run_find_implementations(idx.as_ref(), params, &cache)
        }
        .await;
        record_call(
            &self.state,
            "find_implementations",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    /// Transitive call-graph walk from a root function.
    #[tool(
        description = "Walk the call graph from a function. `direction=\"callers\"` (default) \
                       BFS-walks who calls into `name` up to `max_depth` levels; \
                       `direction=\"callees\"` walks what `name` itself calls. Returns a DAG \
                       (`nodes` + `edges_to` indices). Bounded by `max_depth` (default 3, max 6) \
                       and `max_nodes` (default 100, max 500). Substring-aware? No — `name` is \
                       an exact match against captured call-site identifiers. Use `path` to \
                       disambiguate overloaded names. Cycles detected via name-visited set; \
                       recursive functions surface as a self-edge on the root."
    )]
    pub(crate) async fn call_graph(
        &self,
        Parameters(params): Parameters<CallGraphParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&params).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            let store = self.state.store.read().await;
            let idx = store.index_db.as_ref().cloned();
            drop(store);
            let cache = self.state.cache.load_full();
            run_call_graph(idx.as_ref(), params, &cache)
        }
        .await;
        record_call(
            &self.state,
            "call_graph",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    /// Workdir + branch + HEAD sha.
    #[tool(
        description = "Repository identity: workdir path, current branch name (if HEAD is on one), \
                       full HEAD sha, short HEAD sha. Pairs well with `working_tree_status`."
    )]
    pub(crate) async fn repo_info(
        &self,
        Parameters(_): Parameters<RepoInfoParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = Value::Null;
        let __result: Result<CallToolResult, McpError> = async {
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
        .await;
        record_call(
            &self.state,
            "repo_info",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }
}
