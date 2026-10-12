//! Synthetic LanceDB write benchmark for issue #76 (document tables accumulating ~28k versions).
//!
//! ```text
//! lance_bench <per-doc|batched|incremental> <docs> <chunks-per-doc> <dir>
//! ```
//!
//! `per-doc` replays the pre-fix write pattern (`replace_document` per document, no maintenance);
//! `batched` is the scan path (`replace_documents_many` + post-scan `maintain`); `incremental`
//! then rewrites 1 document at a time 300 times, as the watch path does. Embeddings are zero
//! vectors of dimension 8, so no model is involved. Prints one `key=value` line.
use std::path::{Path, PathBuf};
use std::time::Instant;

use basemind::lance::{BATCH_MAX_DOCS, DocumentRow, LanceStore, PendingBatch, SCAN_RETENTION};

const DIM: u16 = 8;

fn rows(doc: usize, chunks: usize, gen_: u32) -> Vec<DocumentRow> {
    (0..chunks)
        .map(|c| DocumentRow {
            scope: "bench".into(),
            path: format!("docs/d{doc:06}.md"),
            chunk_idx: c as u32,
            mime_type: "text/markdown".into(),
            text: format!("document {doc} chunk {c} generation {gen_} lorem ipsum dolor sit amet"),
            byte_start: (c * 64) as u32,
            byte_end: (c * 64 + 63) as u32,
            embedding: vec![0.0; DIM as usize],
        })
        .collect()
}

fn dir_size(p: &Path) -> u64 {
    std::fs::read_dir(p)
        .map(|rd| {
            rd.flatten()
                .map(|e| match e.metadata() {
                    Ok(m) if m.is_dir() => dir_size(&e.path()),
                    Ok(m) => m.len(),
                    Err(_) => 0,
                })
                .sum()
        })
        .unwrap_or(0)
}

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let (mode, docs, chunks, dir) = (
        a[1].as_str(),
        a[2].parse::<usize>()?,
        a[3].parse::<usize>()?,
        PathBuf::from(&a[4]),
    );
    let store = LanceStore::open(&dir, DIM, "bench")?;
    let t = Instant::now();
    if mode == "per-doc" {
        for d in 0..docs {
            store.replace_document("bench", &format!("docs/d{d:06}.md"), rows(d, chunks, 0))?;
        }
    } else {
        let mut pending = PendingBatch::default();
        for d in 0..docs {
            pending.push("bench", &format!("docs/d{d:06}.md"), rows(d, chunks, 0));
            if pending.is_full() {
                store.replace_documents_many(pending.take())?;
            }
        }
        if !pending.is_empty() {
            store.replace_documents_many(pending.take())?;
        }
        store.maintain(SCAN_RETENTION)?;
    }
    let scan_s = t.elapsed().as_secs_f64();
    let after_scan = (store.version_counts()?, dir_size(&dir));
    let mut inc_s = 0.0;
    if mode == "incremental" {
        let t = Instant::now();
        for i in 0..300 {
            store.replace_documents_many(vec![(
                "bench".into(),
                format!("docs/d{:06}.md", i % docs),
                rows(i % docs, chunks, 1),
            )])?;
        }
        inc_s = t.elapsed().as_secs_f64();
    }
    let fin = (store.version_counts()?, dir_size(&dir));
    println!(
        "mode={mode} docs={docs} chunks={chunks} batch_docs={BATCH_MAX_DOCS} scan_s={scan_s:.1} versions_after_scan={:?} bytes_after_scan={} incr_300_s={inc_s:.1} versions_final={:?} bytes_final={}",
        after_scan.0, after_scan.1, fin.0, fin.1
    );
    Ok(())
}
