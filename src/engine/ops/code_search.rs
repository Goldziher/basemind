//! `code_search`: semantic / hybrid search returning pointers, and its fetch half
//! `basemind code chunk`, which has no MCP tool (`code_read` reads a hit's span).
//!
//! Both are advertised in every build and gate here: a build without the `code-search` feature
//! answers "rebuild with `--features code-search`" instead of an empty result.

#[cfg(not(feature = "code-search"))]
use crate::engine::error::EngineError;
#[cfg(not(feature = "code-search"))]
use crate::engine::types::search::{CodeSearchParams, GetChunkParams};
#[cfg(not(feature = "code-search"))]
use crate::engine::workspace::Workspace;

#[cfg(not(feature = "code-search"))]
impl Workspace {
    /// `code_search` in a build without the retrieval stack.
    pub(crate) fn code_search(&self, _p: CodeSearchParams, _cancelled: &dyn Fn() -> bool) -> Result<(), EngineError> {
        Err(EngineError::feature_missing("code_search", "code-search"))
    }

    /// `basemind code chunk` in a build without the retrieval stack.
    pub(crate) fn code_chunk(&self, _p: GetChunkParams) -> Result<(), EngineError> {
        Err(EngineError::feature_missing("code chunk", "code-search"))
    }
}

#[cfg(feature = "code-search")]
mod enabled {
    use std::sync::atomic::Ordering;

    use crate::engine::budget::budget;
    use crate::engine::error::EngineError;
    use crate::engine::filters::{Filters, grep_selects_no_file, parse_path};
    use crate::engine::ranked::{RANKED_FETCH, RANKED_LIMIT_DEFAULT, page_ranked, ranked_offset, ranked_retrieval};
    use crate::engine::scope::ChangeScope;
    use crate::engine::timing::elapsed_us;
    use crate::engine::types::search::{CodeSearchPage, CodeSearchParams, GetChunkParams, GetChunkResponse};
    use crate::engine::workspace::Workspace;

    impl Workspace {
        /// `code_search`: the ranking of the query (computed once per index generation and kept),
        /// cut to the `changed` scope when one is passed, and the page of it the cursor points at. Stops with
        /// [`EngineError::Cancelled`] once `cancelled` says so: between the lanes, while hits are hydrated
        /// and before the rerank.
        pub(crate) fn code_search(
            &self,
            p: CodeSearchParams,
            cancelled: &dyn Fn() -> bool,
        ) -> Result<CodeSearchPage, EngineError> {
            let started = std::time::Instant::now();
            let generation = self.index_generation.load(Ordering::Relaxed);
            let offset = ranked_offset("code_search", p.cursor.as_ref(), generation)?;
            let limit = p.limit.unwrap_or(RANKED_LIMIT_DEFAULT);
            let path = parse_path("code_search", p.path.as_deref())?;
            let filters = Filters {
                tool: "code_search",
                path: path.as_ref(),
                language: p.language.as_deref(),
            };
            let scope = crate::engine::changed::resolve(self, "code_search", p.changed.as_deref())?;
            let mut arguments = serde_json::json!({
                "query": &p.query,
                "lane": &p.lane,
                "path": &p.path,
                "language": &p.language,
            });
            if let Some(scope) = &scope {
                arguments["scope"] = scope_digest(scope).into();
            }
            let ranking = ranked_retrieval(
                &self.ranked,
                "code_search",
                &arguments,
                generation,
                || {
                    crate::engine::code_search::retrieve(
                        self,
                        p.query,
                        p.lane.as_deref(),
                        RANKED_FETCH as usize,
                        &filters,
                        scope.as_ref(),
                        cancelled,
                    )
                },
                crate::engine::code_search::is_cacheable,
            )?;
            if ranking.hits.is_empty() {
                // A lazy-cache CLI call has an empty map until this barrier builds it, which would
                // make the no-file refusal below see nothing to refuse.
                self.wait_map_ready();
                if let Some(refusal) = grep_selects_no_file(self, &self.cache.load_full(), &filters) {
                    return Err(refusal);
                }
            }
            let mut ranking = ranking;
            let page = page_ranked(
                crate::engine::code_search::merge_overlapping(std::mem::take(&mut ranking.hits)),
                offset,
                limit,
                budget(p.max_tokens),
                generation,
            );
            ranking.hits = page.hits;
            Ok(CodeSearchPage {
                ranking,
                scope_files: scope.as_ref().map(ChangeScope::len),
                budgeted: page.budgeted,
                depth_cut: page.depth_cut,
                next_cursor: page.next_cursor,
                elapsed_us: elapsed_us(started),
                notice: self.lifecycle_notice(),
            })
        }

        /// `basemind code chunk`: one chunk's body, read from the file's `.chunk` sidecar.
        pub(crate) fn code_chunk(&self, p: GetChunkParams) -> Result<GetChunkResponse, EngineError> {
            let started = std::time::Instant::now();
            let blob = {
                let store = self.store_read();
                let entry = store.lookup(&p.path).ok_or_else(|| {
                    EngineError::invalid_argument(format!("`code chunk`: file not indexed: {}", p.path))
                })?;
                let hash_hex = entry.hash_hex.clone();
                store
                    .read_chunks_by_hex(&hash_hex)
                    .map_err(|e| EngineError::internal(format!("`code chunk`: read chunk blob: {e}")))?
                    .ok_or_else(|| {
                        EngineError::invalid_argument(format!(
                            "`code chunk`: no code chunks indexed for {} (scan with --features code-search)",
                            p.path
                        ))
                    })?
            };

            let chunks = &blob.chunks;
            if chunks.is_empty() {
                return Err(EngineError::invalid_argument(format!(
                    "`code chunk`: {} has no chunks",
                    p.path
                )));
            }

            let chunk = if let Some(id) = p.chunk_id.as_deref() {
                chunks.iter().find(|c| c.chunk_id == id).ok_or_else(|| {
                    EngineError::invalid_argument(format!("`code chunk`: chunk_id {id:?} not found in {}", p.path))
                })?
            } else if let Some(bs) = p.byte_start {
                chunks.iter().find(|c| c.byte_start == bs).ok_or_else(|| {
                    EngineError::invalid_argument(format!("`code chunk`: no chunk at byte_start {bs} in {}", p.path))
                })?
            } else if chunks.len() == 1 {
                &chunks[0]
            } else {
                let ids: Vec<&str> = chunks.iter().map(|c| c.chunk_id.as_str()).collect();
                return Err(EngineError::invalid_argument(format!(
                    "`code chunk`: {} has {} chunks; pass `chunk_id` or `byte_start` to disambiguate: {}",
                    p.path,
                    chunks.len(),
                    ids.join(", ")
                )));
            };

            // The sidecar is shared by every copy of this content and names the path chunked first.
            Ok(GetChunkResponse {
                path: p.path.to_string(),
                chunk_id: chunk.chunk_id.clone(),
                symbol: chunk.symbol.clone(),
                kind: chunk.kind.clone(),
                lang: chunk.lang.clone(),
                signature: chunk.signature.clone(),
                doc: chunk.doc.clone(),
                line_start: chunk.line_start,
                line_end: chunk.line_end,
                byte_start: chunk.byte_start,
                byte_end: chunk.byte_end,
                text: chunk.text.clone(),
                elapsed_us: elapsed_us(started),
            })
        }
    }

    /// The files `scope` holds, as one key: a ranking cut to one scope is not another scope's.
    fn scope_digest(scope: &ChangeScope) -> String {
        let mut paths: Vec<&[u8]> = scope.paths().iter().map(crate::path::RelPath::as_bytes).collect();
        paths.sort_unstable();
        let mut hasher = blake3::Hasher::new();
        for path in paths {
            hasher.update(path);
            hasher.update(&[0]);
        }
        hasher.finalize().to_hex().to_string()
    }
}
