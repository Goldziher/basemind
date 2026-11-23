//! `docs_search`: hybrid search over indexed documents (ADR-0029), over the document search engine
//! (`crate::lance::docs_rank`, `crate::lance::docs_page`). This gathers what that engine works over
//! from the workspace (the view's document map, the store, the query embedder, the reranker
//! setting), keeps the ranking for the pages that follow and names the tool in its errors.
//!
//! Advertised in every build: one without the `documents` feature answers "rebuild with
//! `--features documents`" instead of an empty result.

#[cfg(not(feature = "documents"))]
impl crate::engine::workspace::Workspace {
    /// `docs_search` in a build without the document tier.
    pub(crate) fn docs_search(
        &self,
        _p: crate::engine::types::search::DocsSearchParams,
        _cancelled: &dyn Fn() -> bool,
    ) -> Result<(), crate::engine::error::EngineError> {
        Err(crate::engine::error::EngineError::feature_missing(
            "docs_search",
            "documents",
        ))
    }
}

#[cfg(feature = "documents")]
mod enabled {
    use std::sync::atomic::Ordering;

    use crate::engine::budget::budget;
    use crate::engine::cursor::Cursor;
    use crate::engine::error::EngineError;
    use crate::engine::ranked::{RANKED_LIMIT_DEFAULT, ranked_offset, ranked_retrieval};
    use crate::engine::timing::elapsed_us;
    use crate::engine::types::search::{DocsSearchParams, DocsSearchResponse};
    use crate::engine::workspace::Workspace;
    use crate::lance::docs::DocStore;
    use crate::lance::docs_rank::{DocsRequest, DocsSearchError, Filters, LiveDocs, RankInputs, RerankSettings};
    use crate::lance::docs_types::DocsRanking;

    fn tool_error(error: DocsSearchError) -> EngineError {
        let message = format!("`docs_search`: {error}");
        match error {
            DocsSearchError::InvalidArgument(_) => EngineError::InvalidArgument(message),
            DocsSearchError::Unsupported(_) | DocsSearchError::NoLane(_) => EngineError::Unavailable(message),
            DocsSearchError::Store(_) => EngineError::Internal(message),
            DocsSearchError::Cancelled => EngineError::Cancelled,
        }
    }

    impl Workspace {
        /// `docs_search`: the ranking of the query (computed once per index generation and kept),
        /// and the page of it the cursor points at. Stops with [`EngineError::Cancelled`] once
        /// `cancelled` says so, between the ranking's steps and before the page is read: it runs on a
        /// blocking thread that abandoning the call does not stop.
        pub(crate) fn docs_search(
            &self,
            p: DocsSearchParams,
            cancelled: &dyn Fn() -> bool,
        ) -> Result<DocsSearchResponse, EngineError> {
            let started = std::time::Instant::now();
            let generation = self.index_generation.load(Ordering::Relaxed);
            let offset = ranked_offset("docs_search", p.cursor.as_ref(), generation)?;
            let limit = p.limit.unwrap_or(RANKED_LIMIT_DEFAULT);
            let request = DocsRequest {
                query: p.query,
                mime_type: p.mime_type,
                format: p.format,
                entity: p.entity,
                keywords: p.keywords,
                path: p.path,
            };
            let filters =
                Filters::parse(&request, crate::extract::doc_tiers::entity_filter_unavailable()).map_err(tool_error)?;
            // Applied only to an empty answer: a scan run with CLI or environment overrides can have
            // extracted keywords and entities that the served config does not name.
            let refusal = crate::extract::doc_tiers::extractor_filter_refusal(
                &self.config.documents,
                request.entity.as_deref(),
                request.keywords.as_deref().is_some_and(|k| !k.trim().is_empty()),
            );
            let arguments = serde_json::json!({
                "query": &request.query,
                "mime_type": &filters.mime_type,
                "format": &filters.format,
                "entity": &filters.entity,
                "keywords": &filters.keywords,
                "path": &request.path,
            });
            let store = self.docs_store()?;
            let ranking = ranked_retrieval(
                &self.ranked,
                "docs_search",
                &arguments,
                generation,
                || self.rank_docs(&request, &filters, store.as_ref(), cancelled),
                crate::lance::docs_rank::is_cacheable,
            )?;
            if cancelled() {
                return Err(EngineError::Cancelled);
            }
            let paging = crate::lance::docs_page::Paging {
                offset,
                limit,
                max_tokens: budget(p.max_tokens),
                per_doc: p.per_doc.unwrap_or(crate::lance::docs_page::PER_DOC_DEFAULT),
                context: p.context.unwrap_or(false),
            };
            let mut page = crate::lance::docs_page::page(store.as_ref(), ranking, paging);
            if let Some(reason) = refusal.filter(|_| page.hits.is_empty()) {
                return Err(EngineError::invalid_argument(format!("`docs_search`: {reason}")));
            }
            let next_cursor = page
                .next_offset
                .map(|next| Cursor::encode_in_memory(u64::from(next), generation));
            let notice = self.lifecycle_notice();
            if notice.is_some() {
                page.notice = None;
            }
            Ok(DocsSearchResponse {
                page,
                next_cursor,
                elapsed_us: elapsed_us(started),
                notice,
            })
        }

        /// The group's document store for the configured preset, or `None` when no scan has written
        /// one. Opened without the embedding model: the lexical lane needs none.
        fn docs_store(&self) -> Result<Option<DocStore>, EngineError> {
            let preset = &self.config.documents.embedding_preset;
            let dim = crate::scanner_docs::preset_dim(preset)
                .map_err(|error| EngineError::unavailable(format!("`docs_search`: {error}")))?;
            let root = self.store_read().root.clone();
            DocStore::shared_existing(&crate::lance::docs::group_store_dir(&root, preset, dim), dim, preset).map_err(
                |error| EngineError::unavailable(format!("`docs_search`: the document store did not open: {error:#}")),
            )
        }

        /// Rank the query over the view's live documents.
        fn rank_docs(
            &self,
            request: &DocsRequest,
            filters: &Filters,
            store: Option<&DocStore>,
            cancelled: &dyn Fn() -> bool,
        ) -> Result<DocsRanking, EngineError> {
            let (live, owed) = {
                let index = self.store_read();
                (
                    LiveDocs::of_store(&index, filters.path.as_ref()),
                    index.pending_vectors.docs().len(),
                )
            };
            let config = &self.config;
            let embedder = if config.documents.embed {
                self.shared_embedder()
                    .map_err(|error| format!("the embedding model could not load: {error}"))
            } else {
                Err("embeddings are off (`[documents] embed = false`)".to_string())
            };
            let reranker = &config.documents.reranker;
            let rerank = reranker.enabled.then(|| RerankSettings {
                preset: reranker.preset.clone(),
                candidates: reranker.top_k,
                acceleration: crate::embeddings::acceleration(config.resources.onnx_provider),
            });
            let embed = |text: &str| -> Result<Vec<f32>, String> {
                match &embedder {
                    Ok(embedder) => embedder.embed_query(text).map_err(|error| error.to_string()),
                    Err(reason) => Err(reason.clone()),
                }
            };
            let inputs = RankInputs {
                store,
                live,
                owed,
                embed: match &embedder {
                    Ok(_) => Ok(&embed),
                    Err(reason) => Err(reason.clone()),
                },
                rerank,
            };
            crate::lance::docs_rank::rank_until(&request.query, filters, inputs, cancelled).map_err(tool_error)
        }
    }
}
