//! `[documents] max_chunks_per_document` bounds embedding whichever chunker cut the document: a
//! Markdown file xberg chunks past the cap is never sent to the embedder, and keeps its chunks so
//! the lexical lane can still find it. The embedder is a stand-in that counts its calls, so no model
//! is loaded.

#![cfg(feature = "documents")]

use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

use basemind::embeddings::test_document_embedder;
use basemind::extract::doc::{DocConfig, extract_doc};

static CALLS: AtomicUsize = AtomicUsize::new(0);

fn counting(texts: &[&str], dim: u16) -> Result<Vec<Vec<f32>>, String> {
    CALLS.fetch_add(1, Ordering::SeqCst);
    Ok(texts.iter().map(|_| vec![0.5; usize::from(dim)]).collect())
}

fn markdown_doc() -> String {
    (0..60)
        .map(|i| format!("## Section {i}\n\nThe pump station {i} lifts water uphill through the valve.\n\n"))
        .collect()
}

#[test]
fn a_document_xberg_chunks_past_the_cap_is_not_embedded_and_keeps_its_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("guide.md");
    fs::write(&path, markdown_doc()).unwrap();
    let config = DocConfig {
        max_characters: 200,
        overlap: 0,
        max_chunks_per_document: 2,
        ..DocConfig::default()
    };

    test_document_embedder::set(Some(counting));
    let doc = extract_doc(&path, Some("text/markdown"), &config).expect("extract");
    test_document_embedder::set(None);

    assert!(!doc.linear_chunked, "xberg chunked the document");
    assert!(doc.chunks.len() > 2, "over the cap: {} chunks", doc.chunks.len());
    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        0,
        "the embedder ran for a document over the cap"
    );
    assert_eq!(doc.embedding_dim, 0);
    assert!(!doc.embed_failed, "skipping embedding over the cap is not a failure");
    assert!(doc.chunks.iter().all(|chunk| chunk.embedding.is_empty()));
}
