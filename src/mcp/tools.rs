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
        description = "Structural outline of a file: each symbol (name, kind, start row/col) plus \
                       imports. `l2: true` adds calls + doc comments (only if an L2 blob exists for \
                       the current content). `max_tokens` budgets the `symbols` list (not \
                       imports/calls/docs), setting `budgeted`. `format:\"toon\"` for compact rows. \
                       `elapsed_us` = server-side handler latency in µs (excludes transport).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn outline(
        &self,
        Parameters(Lenient(params)): Parameters<Lenient<OutlineParams>>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&params).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            let __body = std::time::Instant::now();
            self.state.await_cache_ready().await;
            fn l1_views(l1: &crate::extract::FileMapL1) -> (Vec<SymbolView>, Vec<ImportView>) {
                let symbols = l1
                    .symbols
                    .iter()
                    .map(|s| SymbolView {
                        name: s.name.clone(),
                        kind: kind_to_str(s.kind),
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
                let store = self.state.store.read().await;
                let l1 = query::file_outline(&store, &params.path)
                    .map_err(|e| McpError::invalid_params(format!("file_outline({}): {e}", params.path), None))?;
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
                    notice: None,
                    // Stamped unconditionally from `__body` just before serialization below.
                    elapsed_us: 0,
                };
                let entry = store
                    .lookup(&params.path)
                    .ok_or_else(|| McpError::internal_error("file not indexed after outline succeeded", None))?;
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
                        r.l2_status = Some("missing — run `basemind query outline <path> --l2` to materialize");
                    }
                    Err(e) => {
                        r.l2_status = Some("error");
                        return Err(McpError::internal_error(format!("read_l2: {e}"), None));
                    }
                }
                r
            } else {
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
                        notice: None,
                        // Stamped unconditionally from `__body` just before serialization below.
                        elapsed_us: 0,
                    }
                } else {
                    let store = self.state.store.read().await;
                    let l1 = query::file_outline(&store, &params.path)
                        .map_err(|e| McpError::invalid_params(format!("file_outline({}): {e}", params.path), None))?;
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
                        notice: None,
                        // Stamped unconditionally from `__body` just before serialization below.
                        elapsed_us: 0,
                    }
                }
            };
            response.notice = self.state.lifecycle_notice();

            if params.max_tokens.is_some() {
                let budgeted = super::budget::apply_budget(std::mem::take(&mut response.symbols), params.max_tokens);
                response.symbols = budgeted.items;
                response.budgeted = budgeted.budgeted;
            }
            response.elapsed_us = elapsed_us(__body);
            super::toon::format_result(&response, super::toon::ResponseFormat::parse(params.format.as_deref()))
        }
        .await;
        record_call(&self.state, "outline", &__params_json, __started, &__result);
        __result
    }

    /// Substring search across symbol names, optionally filtered by kind.
    #[tool(
        description = "Search indexed symbols whose name contains `needle` (case-sensitive \
                       substring). Optional `kind` filter (function/struct/class/...). Up to \
                       `limit` hits (default 100, max 1000): path + line/col + signature. \
                       `total` = matches scanned up to a per-call cap (`limit*64`, min 2000), \
                       NOT the global corpus total; `total_is_partial: true` means the cap was \
                       hit and `total` is a lower bound. `cursor` pages results (invalidate on \
                       rescan, `cursor_invalidated`). `max_tokens` budgets the response (sets \
                       `budgeted` + `next_cursor`). `format:\"toon\"` for compact rows. \
                       `elapsed_us` = server-side handler latency in µs (excludes transport).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn search_symbols(
        &self,
        Parameters(Lenient(params)): Parameters<Lenient<SearchSymbolsParams>>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&params).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            use std::sync::atomic::Ordering;

            let __body = std::time::Instant::now();
            self.state.await_cache_ready().await;
            let format = super::toon::ResponseFormat::parse(params.format.as_deref());
            let kind = params.kind.as_deref().map(parse_kind).transpose()?;
            let limit = params.limit.unwrap_or(SEARCH_LIMIT_DEFAULT).min(SEARCH_LIMIT_MAX) as usize;
            let generation = self.state.cache_generation.load(Ordering::Relaxed);

            let skip = match params.cursor.as_ref() {
                Some(c) => {
                    let (offset, snapshot_id) = c.decode_in_memory()?;
                    if snapshot_id != generation {
                        return super::toon::format_result(
                            &SearchResponse {
                                total: 0,
                                total_is_partial: false,
                                truncated: false,
                                budgeted: false,
                                results: Vec::new(),
                                next_cursor: None,
                                cursor_invalidated: true,
                                notice: self.state.lifecycle_notice(),
                                elapsed_us: elapsed_us(__body),
                            },
                            format,
                        );
                    }
                    offset as usize
                }
                None => 0,
            };

            if params.needle.is_empty() {
                return super::toon::format_result(
                    &SearchResponse {
                        total: 0,
                        total_is_partial: false,
                        truncated: false,
                        budgeted: false,
                        results: Vec::new(),
                        next_cursor: None,
                        cursor_invalidated: false,
                        notice: self.state.lifecycle_notice(),
                        elapsed_us: elapsed_us(__body),
                    },
                    format,
                );
            }
            let finder = memchr::memmem::Finder::new(params.needle.as_bytes());
            let max_total = search_max_total(limit);
            let mut results: Vec<SearchHitView> = Vec::with_capacity(limit);
            let mut total: usize = 0;
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
                            kind: kind_to_str(sym.kind),
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
            let budget = super::budget::apply_budget(results, params.max_tokens);
            let results = budget.items;
            let budgeted = budget.budgeted;
            let next_cursor = if total > results.len() {
                Some(super::cursor::Cursor::encode_in_memory(
                    (skip + results.len()) as u64,
                    generation,
                ))
            } else {
                None
            };
            super::toon::format_result(
                &SearchResponse {
                    total,
                    total_is_partial,
                    truncated,
                    budgeted,
                    results,
                    next_cursor,
                    cursor_invalidated: false,
                    notice: self.state.lifecycle_notice(),
                    elapsed_us: elapsed_us(__body),
                },
                format,
            )
        }
        .await;
        record_call(&self.state, "search_symbols", &__params_json, __started, &__result);
        __result
    }

    /// List indexed files, optionally filtered by path substring and/or language.
    #[tool(
        description = "List indexed files with language + size. Optional `path_contains` substring \
                       and `language` filter (rust/python/typescript/tsx/javascript/go). Default \
                       limit 200, max 5000 (a larger request is clamped, setting \
                       `limit_clamped`). `cursor` pages results (invalidate on rescan, \
                       `cursor_invalidated`). `max_tokens` budgets the response (sets `budgeted` \
                       + `next_cursor`). `format:\"toon\"` for compact rows. \
                       `elapsed_us` = server-side handler latency in µs (excludes transport).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn list_files(
        &self,
        Parameters(params): Parameters<ListFilesParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&params).unwrap_or(Value::Null);
        let __result = run_list_files(&self.state, params).await;
        record_call(&self.state, "list_files", &__params_json, __started, &__result);
        __result
    }

    /// Fuzzy filename/path search over indexed paths — an fzf/fd-style replacement for shell `find`.
    #[tool(
        description = "Fuzzy subsequence match of `query` against every indexed path (fzf/fd-style: \
                       letters of `query` must appear in order, not necessarily contiguous; \
                       case-insensitive). Ranked by `nucleo-matcher` score (path-aware bonuses for \
                       `/`-boundary and prefix hits); non-matches are dropped, not just scored low. \
                       Optional `path_prefix` and `language` filters are applied before scoring. \
                       Name-only heuristic — no scope/import resolution. Default limit 200, max \
                       5000 (a larger request is clamped, setting `limit_clamped`). `cursor` pages \
                       results (invalidated on rescan, `cursor_invalidated`). `max_tokens` budgets \
                       the response (sets `budgeted` + `next_cursor`). `format:\"toon\"` for compact \
                       rows. \
                       `elapsed_us` = server-side handler latency in µs (excludes transport).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn find_files(
        &self,
        Parameters(Lenient(params)): Parameters<Lenient<FindFilesParams>>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&params).unwrap_or(Value::Null);
        let __result = run_find_files(&self.state, params).await;
        record_call(&self.state, "find_files", &__params_json, __started, &__result);
        __result
    }

    /// Heuristic reverse-dependency lookup via import statements.
    #[tool(
        description = "Indexed files whose imports mention `module`. Heuristic: substring match \
                       against each import's recorded module path. \
                       `elapsed_us` = server-side handler latency in µs (excludes transport).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn dependents(
        &self,
        Parameters(Lenient(params)): Parameters<Lenient<DependentsParams>>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&params).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            let __body = std::time::Instant::now();
            self.state.await_cache_ready().await;
            let paths: Vec<crate::path::RelPath> =
                crate::extract::l3::dependents_of(&params.module, &self.state.cache.load().imports_index)
                    .into_iter()
                    .map(|p| crate::path::RelPath::from(p.as_path()))
                    .collect();
            json_result(&DependentsResponse {
                module: params.module.clone(),
                paths,
                notice: self.state.lifecycle_notice(),
                elapsed_us: elapsed_us(__body),
            })
        }
        .await;
        record_call(&self.state, "dependents", &__params_json, __started, &__result);
        __result
    }

    /// High-level repo + cache state.
    #[tool(
        description = "Indexed-repo report: file count, on-disk `blob_count`, total bytes, \
                       per-language breakdown, root path, grammar cache directory, schema \
                       version. A `note` appears when the view index is empty but blobs exist \
                       (lost index — rescan). \
                       `elapsed_us` = server-side handler latency in µs (excludes transport).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn status(&self, Parameters(_): Parameters<StatusParams>) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = Value::Null;
        let __result: Result<CallToolResult, McpError> = async {
            let __body = std::time::Instant::now();
            let indexing = self
                .state
                .initial_scan_active
                .load(std::sync::atomic::Ordering::Relaxed);
            let index_build_ms = {
                let ms = self.state.initial_scan_ms.load(std::sync::atomic::Ordering::Relaxed);
                (ms > 0).then_some(ms)
            };
            let warming = self.state.cache_warming.load(std::sync::atomic::Ordering::Relaxed);
            let warm_ms = {
                let ms = self.state.cache_warm_ms.load(std::sync::atomic::Ordering::Relaxed);
                (ms > 0).then_some(ms)
            };
            let notice = self.state.lifecycle_notice();
            let store = match self.state.store.try_read() {
                Ok(store) => store,
                Err(_) => {
                    return json_result(&StatusResponse {
                        file_count: 0,
                        blob_count: count_fm_blobs(),
                        note: Some(
                            "a rebuild is in progress (another basemind process holds the store \
                             lock); index counts are unavailable until it completes"
                                .to_string(),
                        ),
                        rebuild_in_progress: true,
                        indexing,
                        index_build_ms,
                        warming,
                        warm_ms,
                        notice,
                        total_size_bytes: 0,
                        languages: BTreeMap::new(),
                        cache_dir: crate::lang::grammar_cache_dir()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "(unresolved)".to_string()),
                        schema_version: crate::extract::SCHEMA_VER,
                        root: self.state.root.display().to_string(),
                        submodules: self
                            .state
                            .repo
                            .as_ref()
                            .map(|r| r.submodule_paths())
                            .unwrap_or_default(),
                        elapsed_us: elapsed_us(__body),
                    });
                }
            };
            let mut by_lang_ref: BTreeMap<&str, usize> = BTreeMap::new();
            let mut total_size: u64 = 0;
            for entry in store.index.files.values() {
                *by_lang_ref.entry(entry.language.as_str()).or_insert(0) += 1;
                total_size = total_size.saturating_add(entry.size_bytes);
            }
            let by_lang: BTreeMap<String, usize> = by_lang_ref.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
            let cache_dir = crate::lang::grammar_cache_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(unresolved)".to_string());
            let submodules = self
                .state
                .repo
                .as_ref()
                .map(|r| r.submodule_paths())
                .unwrap_or_default();
            let file_count = store.index.files.len();
            let blob_count = count_fm_blobs();
            let note = blob_divergence_note(file_count, blob_count);
            json_result(&StatusResponse {
                file_count,
                blob_count,
                note,
                rebuild_in_progress: false,
                indexing,
                index_build_ms,
                warming,
                warm_ms,
                notice,
                total_size_bytes: total_size,
                languages: by_lang,
                cache_dir,
                schema_version: crate::extract::SCHEMA_VER,
                root: self.state.root.display().to_string(),
                submodules,
                elapsed_us: elapsed_us(__body),
            })
        }
        .await;
        record_call(&self.state, "status", &__params_json, __started, &__result);
        __result
    }

    /// Incoming call sites for any callee whose identifier contains `name`.
    #[tool(
        description = "Call sites whose callee identifier contains `name` (case-sensitive \
                       substring). Fjall-backed over L2 captures; hits are (path, line, column, \
                       exact callee). Name-only, no scope resolution: `Foo::bar()` and `bar()` \
                       both match name=\"bar\". Up to `limit` hits (default 100, max 1000); scan \
                       bounded by `scan_cap = limit * 8`. Needs `eager_l2=true` (default). \
                       `cursor` pages results. `max_tokens` budgets the response (sets `budgeted` \
                       + `next_cursor`). `format:\"toon\"` for compact rows. \
                       `elapsed_us` = server-side handler latency in µs (excludes transport).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn find_references(
        &self,
        Parameters(Lenient(params)): Parameters<Lenient<FindReferencesParams>>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&params).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            let __body = std::time::Instant::now();
            self.state.await_cache_ready().await;
            let store = self.state.store.read().await;
            let idx = store.index_db.as_ref().cloned();
            drop(store);
            let cache = self.state.cache.load_full();
            run_find_references(idx.as_ref(), params, &cache, self.state.lifecycle_notice(), __body)
        }
        .await;
        record_call(&self.state, "find_references", &__params_json, __started, &__result);
        __result
    }

    /// Callers of a specific definition (path + name + optional kind).
    #[tool(
        description = "Call sites of a specific definition (`path` + `name` + optional kind). \
                       Resolves it via the symbols index (echoed in `definition`), then PREFERS \
                       scope/import-resolved edges: when the definition resolves, returns only \
                       precise, scope-correct callers (never conflates same-named symbols) and \
                       sets `resolved: true` — cross-file callers come from the index (read locally, \
                       or forwarded to the machine daemon on a read-only serve, so precise \
                       resolution holds there too). \
                       Falls back to the same name-based scan as `find_references` (name-only, \
                       no-scope) when nothing resolves. Default limit 100, max 1000. `cursor` \
                       pages the name-scan fallback (resolved results return in one page). \
                       `max_tokens` budgets the response (sets `budgeted` + `next_cursor`). \
                       `elapsed_us` = server-side handler latency in µs (excludes transport).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn find_callers(
        &self,
        Parameters(Lenient(params)): Parameters<Lenient<FindCallersParams>>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&params).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            let __body = std::time::Instant::now();
            self.state.await_cache_ready().await;
            let store = self.state.store.read().await;
            let cache = self.state.cache.load_full();
            // A `daemon_writer` serve has no open index, so the precise cross-file resolution forwards
            // to the daemon (the sole fjall writer); every other serve resolves locally.
            #[cfg(all(feature = "comms", any(unix, windows)))]
            let refs = if self.state.daemon_writer {
                let client = super::helpers_comms::resolve_comms_client(&self.state, None).await?;
                RefsSource::Daemon {
                    client,
                    root: self.state.root.clone(),
                }
            } else {
                RefsSource::Local(&store)
            };
            #[cfg(not(all(feature = "comms", any(unix, windows))))]
            let refs = RefsSource::Local(&store);
            run_find_callers(
                &store,
                refs,
                &self.state.root,
                &cache,
                params,
                self.state.lifecycle_notice(),
                __body,
            )
            .await
        }
        .await;
        record_call(&self.state, "find_callers", &__params_json, __started, &__result);
        __result
    }

    /// Resolve a reference position to its scope/import-resolved definition.
    #[tool(
        description = "Resolve the reference at `path`:`line`:`column` to its definition — \
                       scope-resolved, NOT name-matched, so it never conflates same-named symbols. \
                       Returns the definition `{path, line, column, name}` (`path` may be another \
                       file — cross-file imports are followed for JS/TS), or omits `definition` \
                       when the position holds no resolved binding (module-global, unresolved, or a \
                       language without resolution coverage). `line` is 1-based, `column` 0-based \
                       bytes; any byte inside the identifier resolves for span-aware engines (oxc \
                       JS/TS), the tree-sitter `locals` fallback + the cross-file hop match the \
                       identifier's start byte. The in-file hop reads the content-addressed blobs \
                       (answers even in a read-only session); the cross-file hop reads the index — \
                       locally, or forwarded to the machine daemon on a read-only serve. \
                       `elapsed_us` = server-side handler latency in µs (excludes transport).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn goto_definition(
        &self,
        Parameters(Lenient(params)): Parameters<Lenient<GotoDefinitionParams>>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&params).unwrap_or(Value::Null);
        let __result = super::helpers_intel::run_goto_definition(&self.state, params).await;
        record_call(&self.state, "goto_definition", &__params_json, __started, &__result);
        __result
    }

    /// Regex content search across indexed files.
    #[tool(
        description = "Regex search across indexed files (`pattern` is Rust regex syntax). Returns \
                       line + column + matched text plus optional 1-line context. Prefer \
                       `search_symbols` for a plain substring identifier (index-backed, faster). \
                       Bounded by `scan_cap = limit * 8` files; narrow with `language` / \
                       `path_contains`. Default limit 100, max 1000. `cursor` pages results \
                       (invalidate on rescan). `max_tokens` budgets the response (sets `budgeted` \
                       + `next_cursor`). `format:\"toon\"` for compact rows. \
                       `elapsed_us` = server-side handler latency in µs (excludes transport).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn workspace_grep(
        &self,
        Parameters(Lenient(params)): Parameters<Lenient<WorkspaceGrepParams>>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&params).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            let __body = std::time::Instant::now();
            self.state.await_cache_ready().await;
            run_workspace_grep(&self.state, params, __body)
        }
        .await;
        record_call(&self.state, "workspace_grep", &__params_json, __started, &__result);
        __result
    }

    /// Types / classes that implement, extend, or inherit from a name containing `trait_name`.
    #[tool(
        description = "Types that implement/extend/inherit `trait_name` (trait / interface / base \
                       class). Returns (trait, implementor, file, line, column). `trait_name` is a \
                       case-sensitive substring match (full-partition scan). Covers Rust, Python, \
                       TS/TSX, JS class/interface extends/implements; Go structural satisfaction \
                       not detected. Bounded by `scan_cap = limit * 8`. `cursor` pages results \
                       (Fjall-backed, stable across rescans). `max_tokens` budgets the response \
                       (sets `budgeted` + `next_cursor`). \
                       `elapsed_us` = server-side handler latency in µs (excludes transport).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn find_implementations(
        &self,
        Parameters(Lenient(params)): Parameters<Lenient<FindImplementationsParams>>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&params).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            let __body = std::time::Instant::now();
            self.state.await_cache_ready().await;
            let store = self.state.store.read().await;
            let idx = store.index_db.as_ref().cloned();
            drop(store);
            let cache = self.state.cache.load_full();
            run_find_implementations(idx.as_ref(), params, &cache, self.state.lifecycle_notice(), __body)
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
        description = "BFS the call graph from a function. `direction=\"callers\"` (default) walks \
                       who calls `name`; `\"callees\"` walks what `name` calls. Returns a DAG \
                       (`nodes` + `edges_to` indices). Bounded by `max_depth` (default 3, max 6) \
                       and `max_nodes` (default 100, max 500). `name` is exact (not substring); \
                       use `path` to disambiguate overloads. Cycles detected; recursion surfaces \
                       as a self-edge on the root. \
                       `elapsed_us` = server-side handler latency in µs (excludes transport).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn call_graph(
        &self,
        Parameters(params): Parameters<CallGraphParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&params).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            let __body = std::time::Instant::now();
            self.state.await_cache_ready().await;
            let store = self.state.store.read().await;
            let idx = store.index_db.as_ref().cloned();
            drop(store);
            let cache = self.state.cache.load_full();
            run_call_graph(idx.as_ref(), params, &cache, self.state.lifecycle_notice(), __body)
        }
        .await;
        record_call(&self.state, "call_graph", &__params_json, __started, &__result);
        __result
    }

    /// Workdir + branch + HEAD sha.
    #[tool(
        description = "Repository identity: workdir path, current branch (if HEAD is on one), full \
                       + short HEAD sha. Pairs with `working_tree_status`. \
                       `elapsed_us` = server-side handler latency in µs (excludes transport).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn repo_info(
        &self,
        Parameters(_): Parameters<RepoInfoParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = Value::Null;
        let __result: Result<CallToolResult, McpError> = async {
            let __body = std::time::Instant::now();
            let repo = require_git_repo(&self.state)?;
            let info = repo
                .info()
                .map_err(|e| McpError::internal_error(format!("repo info: {e}"), None))?;
            json_result(&RepoInfoResponse {
                workdir: info.workdir.display().to_string(),
                head_sha: info.head_sha,
                head_short_sha: info.head_short_sha,
                branch: info.branch,
                elapsed_us: elapsed_us(__body),
            })
        }
        .await;
        record_call(&self.state, "repo_info", &__params_json, __started, &__result);
        __result
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

/// The `search_symbols` scan cap: matches walked are bounded by this so a common needle
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
    let blobs_dir = crate::store::global_blobs_dir();
    let Ok(entries) = std::fs::read_dir(&blobs_dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.ends_with(".fm.msgpack")))
        .count()
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
