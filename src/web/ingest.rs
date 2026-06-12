//! Shared chunk + embed + LanceDB write path for web ingestion.
//!
//! Called by both `web_scrape` (single page) and `web_crawl` (each page) so
//! the LanceDB row shape stays identical to documents indexed from disk.
//!
//! The flow mirrors `scanner_docs::extract_and_persist_doc`:
//!  1. chunk the page text via `kreuzberg::chunking::chunk_text`,
//!  2. embed each chunk via the shared `SharedEmbedder`,
//!  3. write the rows to LanceDB through `LanceStore::replace_document`.
//!
//! Errors during embed / write are returned to the caller rather than logged
//! and swallowed — the MCP tool wants to report `chunks_indexed = 0` plus the
//! reason to the agent, not silently succeed.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use kreuzberg::chunking::{ChunkingConfig, chunk_text};

use crate::config::DocumentsConfig;
use crate::embeddings::SharedEmbedder;
use crate::lance::{DocumentRow, LanceStore};

/// Outcome of indexing a single fetched page.
#[derive(Debug, Clone)]
pub struct IndexedPage {
    /// Number of chunks written to LanceDB.
    pub chunks_indexed: usize,
    /// Source byte length before chunking. Zero when no body / non-text content.
    pub bytes: usize,
}

/// Chunk `body`, embed each chunk, replace all rows for `(scope, path)` in
/// LanceDB, return the count. Empty body short-circuits with `chunks_indexed=0`.
///
/// `documents_cfg` controls chunk sizing (max_characters, overlap) so web
/// chunking matches disk chunking — agents see consistent retrieval behaviour
/// across both sources.
pub fn index_page(
    lance: &LanceStore,
    embedder: &Arc<SharedEmbedder>,
    documents_cfg: &DocumentsConfig,
    scope: &str,
    path: &str,
    mime_type: &str,
    body: &str,
) -> Result<IndexedPage> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        // Drop any prior rows for this URL so a re-scrape that now yields empty
        // text doesn't leave stale chunks behind. `replace_document` with an
        // empty Vec performs the delete and returns Ok.
        lance
            .replace_document(scope, path, Vec::new())
            .context("clear stale rows for empty body")?;
        return Ok(IndexedPage {
            chunks_indexed: 0,
            bytes: 0,
        });
    }

    let chunking_cfg = ChunkingConfig {
        max_characters: documents_cfg.max_characters,
        overlap: documents_cfg.overlap,
        ..Default::default()
    };
    let chunked = chunk_text(body, &chunking_cfg, None).context("chunk_text on web page body")?;

    if chunked.chunks.is_empty() {
        lance
            .replace_document(scope, path, Vec::new())
            .context("clear stale rows when chunker yielded zero chunks")?;
        return Ok(IndexedPage {
            chunks_indexed: 0,
            bytes: body.len(),
        });
    }

    let dim = embedder.dim();
    if lance.dim() != dim {
        return Err(anyhow!(
            "LanceStore dim {} disagrees with embedder dim {}",
            lance.dim(),
            dim
        ));
    }

    let mut rows: Vec<DocumentRow> = Vec::with_capacity(chunked.chunks.len());
    for (idx, chunk) in chunked.chunks.iter().enumerate() {
        let embedding = embedder
            .embed(&chunk.content)
            .with_context(|| format!("embed chunk {idx} of {path}"))?;
        if embedding.len() != usize::from(dim) {
            return Err(anyhow!(
                "embedder returned vector of length {} but dim is {}",
                embedding.len(),
                dim
            ));
        }
        let byte_start = u32::try_from(chunk.metadata.byte_start).unwrap_or(u32::MAX);
        let byte_end = u32::try_from(chunk.metadata.byte_end).unwrap_or(u32::MAX);
        rows.push(DocumentRow {
            scope: scope.to_string(),
            path: path.to_string(),
            chunk_idx: u32::try_from(idx).unwrap_or(u32::MAX),
            mime_type: mime_type.to_string(),
            text: chunk.content.clone(),
            byte_start,
            byte_end,
            embedding,
        });
    }

    let count = rows.len();
    lance
        .replace_document(scope, path, rows)
        .with_context(|| format!("write {count} chunks to LanceDB for {path}"))?;

    Ok(IndexedPage {
        chunks_indexed: count,
        bytes: body.len(),
    })
}

/// Default scope tag for web content when the caller does not override it.
/// Falls back to `"web:unknown"` when the URL has no host (which the `Url`
/// newtype's parser does not actually permit for http/https — kept as a
/// defence-in-depth string).
pub fn default_scope(url: &crate::url::Url) -> String {
    let host = url.host_str().unwrap_or("unknown");
    format!("web:{host}")
}

#[cfg(test)]
mod tests {
    use super::default_scope;
    use crate::url::Url;

    #[test]
    fn default_scope_uses_host_for_simple_url() {
        let u = Url::parse("https://example.com/page").unwrap();
        assert_eq!(default_scope(&u), "web:example.com");
    }

    #[test]
    fn default_scope_distinguishes_subdomains() {
        let a = Url::parse("https://docs.rs/rmcp/").unwrap();
        let b = Url::parse("https://github.com/Goldziher/basemind").unwrap();
        assert_eq!(default_scope(&a), "web:docs.rs");
        assert_eq!(default_scope(&b), "web:github.com");
        assert_ne!(default_scope(&a), default_scope(&b));
    }

    #[test]
    fn default_scope_strips_port_and_path() {
        // The scope is host-only — port + path are deliberately excluded so
        // `search_documents { scope: "web:example.com" }` retrieves every
        // page from that host regardless of port. Two URLs to the same host
        // on different ports collapse into one scope.
        let a = Url::parse("https://example.com:8443/a").unwrap();
        let b = Url::parse("https://example.com/b?q=1").unwrap();
        assert_eq!(default_scope(&a), default_scope(&b));
        assert_eq!(default_scope(&a), "web:example.com");
    }

    #[test]
    fn default_scope_preserves_case_as_parsed() {
        // `url::Url` lowercases the host at parse time, so the scope tag is
        // already canonical — locking in that contract here so a future
        // refactor doesn't accidentally start round-tripping mixed case.
        let u = Url::parse("https://EXAMPLE.com/").unwrap();
        assert_eq!(default_scope(&u), "web:example.com");
    }

    #[test]
    fn default_scope_handles_ipv4_host() {
        let u = Url::parse("http://192.168.1.1/").unwrap();
        assert_eq!(default_scope(&u), "web:192.168.1.1");
    }

    #[test]
    fn default_scope_handles_ipv6_host() {
        let u = Url::parse("http://[::1]/").unwrap();
        // url::Url returns IPv6 hosts bracketed-or-not depending on form; we
        // accept either as long as it round-trips with the scope prefix.
        let scope = default_scope(&u);
        assert!(
            scope.starts_with("web:") && scope.contains(":1"),
            "ipv6 scope should contain the address; got {scope}"
        );
    }
}
