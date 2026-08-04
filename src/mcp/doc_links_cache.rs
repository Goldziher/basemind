//! Document→code link (ADR-0008) load + attach for the in-RAM [`MapCache`].
//!
//! Centralizes the LanceDB read so EVERY cache-(re)build path attaches the persisted links, not just
//! the boot preload. Before this, only [`super::background::spawn_cache_warm`] and the view watcher
//! called the loader; every other rebuild (`daemon_forward::refresh_cache_after_scan`, an unscoped
//! `scan_and_refresh`, `build_cache_on_demand`, the eager boot) published a cache with empty
//! `doc_links`, so the `documents` graph lane silently went empty after the first rescan.
//!
//! The load runs the `LanceStore`'s internal `block_on`, so callers MUST invoke [`attach`] off the
//! async reactor (a `spawn_blocking` / `block_in_place` / plain std-thread context). Incremental
//! [`MapCache::with_delta`](super::MapCache::with_delta) rescans do not reload — they carry the
//! previous cache's links forward — so `attach` runs only on a full, blocking rebuild.

#[cfg(feature = "documents")]
use std::path::Path;

use super::MapCache;
use crate::config::Config;
use crate::store::Store;

/// Load the persisted document→code links (ADR-0008) for `scope` from the LanceDB document store,
/// mapping each stored row back into a [`codegraph::DocLink`](super::codegraph::DocLink).
///
/// Opens the store read-side with the configured embedding preset's dim (matching how the scanner
/// wrote it), so links load even on an embedding-free store. Returns empty — never fails a cache
/// build — when the lance dir is absent or a read errors: the `documents` graph lane then simply has
/// no edges.
#[cfg(feature = "documents")]
fn load(basemind_dir: &Path, scope: &str, preset: &str) -> Vec<super::codegraph::DocLink> {
    use super::codegraph::{DocLink, DocMention};

    let lance_dir = basemind_dir.join(crate::store::LANCE_DIR);
    if !lance_dir.exists() {
        return Vec::new();
    }
    let dim = match crate::scanner_docs::preset_dim(preset) {
        Ok(dim) => dim,
        Err(error) => {
            tracing::warn!(?error, preset = %preset, "doc links load: unknown preset; skipping");
            return Vec::new();
        }
    };
    let lance = match crate::lance::LanceStore::open(&lance_dir, dim, preset) {
        Ok(lance) => lance,
        Err(error) => {
            tracing::warn!(?error, "doc links load: open LanceStore failed; skipping");
            return Vec::new();
        }
    };
    let rows = match lance.all_doc_links(scope) {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(?error, "doc links load: query failed; skipping");
            return Vec::new();
        }
    };
    rows.into_iter()
        .map(|r| DocLink {
            doc_path: crate::path::RelPath::from(r.doc_path.as_str()),
            chunk_idx: r.chunk_idx,
            mention: if r.mention_kind == "path" {
                DocMention::Path(crate::path::RelPath::from(r.mention_value.as_str()))
            } else {
                DocMention::Name(r.mention_value)
            },
        })
        .collect()
}

/// Attach the persisted document→code links to a freshly built [`MapCache`] before it is published.
///
/// MUST be called off the async reactor (see the module docs) — the `LanceStore`'s internal
/// `block_on` must not nest inside a runtime worker. A no-op without the `documents` feature; the
/// `&mut cache` / `&store` / `&config` arguments keep the bindings "used" so the default build stays
/// warning-free without an `#[allow]`.
#[cfg(feature = "documents")]
pub(crate) fn attach(cache: &mut MapCache, store: &Store, config: &Config, scope: &str) {
    cache.doc_links = load(&store.basemind_dir, scope, &config.documents.embedding_preset).into();
}

#[cfg(not(feature = "documents"))]
pub(crate) fn attach(_cache: &mut MapCache, _store: &Store, _config: &Config, _scope: &str) {}

/// Attach the persisted links from a caller already ON the async reactor (an inline `scan_and_refresh`
/// / `build_cache_on_demand`). The `LanceStore`'s `block_on` cannot run on a reactor worker, so the
/// load is offloaded to [`tokio::task::spawn_blocking`] — the only difference from [`attach`], which
/// is for callers that are already off-reactor. A no-op without the `documents` feature.
#[cfg(feature = "documents")]
pub(crate) async fn attach_async(cache: &mut MapCache, store: &Store, config: &Config, scope: &str) {
    let basemind_dir = store.basemind_dir.clone();
    let scope = scope.to_string();
    let preset = config.documents.embedding_preset.clone();
    cache.doc_links = tokio::task::spawn_blocking(move || load(&basemind_dir, &scope, &preset))
        .await
        .unwrap_or_default()
        .into();
}

#[cfg(not(feature = "documents"))]
pub(crate) async fn attach_async(_cache: &mut MapCache, _store: &Store, _config: &Config, _scope: &str) {}
