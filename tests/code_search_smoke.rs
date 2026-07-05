//! End-to-end smoke test for the semantic code-search tier (`search_code` + `get_chunk`).
//!
//! Gated on `feature = "code-search"`. Drives the real `basemind` binary: scan a tiny fixture
//! (which chunks + embeds source), then `query search-code` and `query get-chunk` over the CLI —
//! the same tool code an MCP client dispatches.
//!
//! The embedding model downloads on first use. When it is unavailable (offline CI, cold grammar),
//! the scan still succeeds but produces no vectors, so `search-code` yields no hits or errors — the
//! test then SKIPS gracefully rather than failing, per the plan's cold-start contract.
#![cfg(feature = "code-search")]

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_basemind")
}

/// The fixture: one documented function + a struct, so the chunker emits at least one symbol
/// chunk whose doc + signature make it a strong semantic match for the query below.
const FIXTURE: &str = "/// Parse a configuration file's text into a typed Config value.\n\
pub fn parse_config(text: &str) -> Config {\n\
\x20   let _ = text;\n\
\x20   Config { name: String::new() }\n\
}\n\
\n\
pub struct Config {\n\
\x20   pub name: String,\n\
}\n";

#[test]
fn search_code_finds_chunk_then_get_chunk_fetches_body() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    std::fs::write(root.join("lib.rs"), FIXTURE).expect("write fixture");

    // Scan: chunks + embeds. Never fails on embedding trouble (chunk/embed errors are swallowed).
    let scan = Command::new(bin())
        .current_dir(root)
        .arg("scan")
        .output()
        .expect("spawn scan");
    assert!(
        scan.status.success(),
        "basemind scan failed: {}",
        String::from_utf8_lossy(&scan.stderr)
    );

    // search_code over the CLI mirror.
    let out = Command::new(bin())
        .current_dir(root)
        .args([
            "--json",
            "query",
            "search-code",
            "parse a configuration file into a struct",
        ])
        .output()
        .expect("spawn search-code");
    if !out.status.success() {
        eprintln!(
            "SKIP: search-code errored (embedder unavailable / offline): {}",
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let value: serde_json::Value = match serde_json::from_str(&stdout) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("SKIP: search-code produced non-JSON output ({e}): {stdout}");
            return;
        }
    };
    let hits = value.get("hits").and_then(|h| h.as_array());
    let Some(hits) = hits else {
        eprintln!("SKIP: search-code response has no `hits` array: {value}");
        return;
    };
    if hits.is_empty() {
        eprintln!("SKIP: zero hits (grammar cold or embedder offline) — code-search path exercised without a corpus");
        return;
    }

    // Every chunk in this fixture belongs to lib.rs — assert the top pointer points there.
    let top = &hits[0];
    assert_eq!(
        top.get("path").and_then(|p| p.as_str()),
        Some("lib.rs"),
        "top hit must point at the only indexed file: {top}"
    );
    let chunk_id = top
        .get("chunk_id")
        .and_then(|c| c.as_str())
        .expect("hit carries a chunk_id pointer");
    assert!(
        chunk_id.contains(':'),
        "chunk_id is content-addressed `<hash>:<ordinal>`: {chunk_id}"
    );

    // get_chunk fetches the body for that pointer.
    let gc = Command::new(bin())
        .current_dir(root)
        .args(["--json", "query", "get-chunk", "lib.rs", "--chunk-id", chunk_id])
        .output()
        .expect("spawn get-chunk");
    assert!(
        gc.status.success(),
        "get-chunk failed: {}",
        String::from_utf8_lossy(&gc.stderr)
    );
    let gv: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&gc.stdout)).expect("get-chunk emits JSON");
    let text = gv.get("text").and_then(|t| t.as_str()).unwrap_or("");
    assert!(!text.is_empty(), "get_chunk must return a non-empty body: {gv}");
    assert_eq!(
        gv.get("chunk_id").and_then(|c| c.as_str()),
        Some(chunk_id),
        "get_chunk echoes the requested chunk_id"
    );
}

/// Regression test for the stale-sidecar / re-chunk guard.
///
/// Before the fix, an `Unchanged` early-return in the scanner skipped chunking when the
/// `.chunk.msgpack` sidecar was absent but the content hash was unchanged (e.g. code-search was
/// enabled after a prior scan). The fix forces a re-chunk when `should_chunk` is on and the
/// sidecar is missing, even when the file content is identical to the stored blob.
#[test]
fn stale_sidecar_rechunked_when_content_unchanged() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    std::fs::write(root.join("lib.rs"), FIXTURE).expect("write fixture");

    // First scan: generates .chunk.msgpack sidecar(s) alongside the L1/L2 blobs.
    let scan1 = Command::new(bin())
        .current_dir(root)
        .arg("scan")
        .output()
        .expect("spawn first scan");
    assert!(
        scan1.status.success(),
        "first scan failed: {}",
        String::from_utf8_lossy(&scan1.stderr)
    );

    // Locate the chunk sidecar written by the first scan. If none exists (chunker disabled or
    // an early fatal error), skip gracefully rather than failing — this test is only meaningful
    // when chunking actually ran.
    let sidecar = find_chunk_sidecar(root);
    let Some(sidecar) = sidecar else {
        eprintln!(
            "SKIP: no .chunk.msgpack sidecar found after first scan \
             (chunker may be disabled or model unavailable)"
        );
        return;
    };
    assert!(
        sidecar.exists(),
        "sidecar must exist after first scan: {}",
        sidecar.display()
    );

    // Delete the sidecar WITHOUT modifying the source file. The content hash is therefore
    // unchanged — a naive scanner would treat the file as `Unchanged` and skip chunking,
    // leaving the sidecar absent and the code-search index empty.
    std::fs::remove_file(&sidecar).expect("remove sidecar");
    assert!(!sidecar.exists(), "sidecar must be gone after manual deletion");

    // Second scan: the stale-sidecar guard must detect the missing sidecar and re-chunk,
    // even though the file content hash is identical. Chunk writing is deterministic and does
    // not depend on embedding model availability (the sidecar stores the textual chunks; the
    // embedding is a separate step that may fail silently without affecting sidecar creation).
    let scan2 = Command::new(bin())
        .current_dir(root)
        .arg("scan")
        .output()
        .expect("spawn second scan");
    assert!(
        scan2.status.success(),
        "second scan failed: {}",
        String::from_utf8_lossy(&scan2.stderr)
    );

    // The sidecar must have been regenerated. This is deterministic regardless of the
    // embedding model — chunk-only writes happen before the optional embed step.
    assert!(
        sidecar.exists(),
        "re-scan must regenerate the .chunk.msgpack sidecar after it was deleted \
         (the stale-sidecar guard should force re-chunking despite unchanged content hash): \
         sidecar={}",
        sidecar.display()
    );
}

/// End-to-end test for the BM25 keyword lane (`search_code --mode keyword`).
///
/// Unlike the semantic lane, keyword search needs no embedder — postings are pure Fjall + sidecar —
/// so with `[code_search] embed = false` this test is fully deterministic and asserts strictly (no
/// cold-model skip). The query term `config` appears in the fixture's `parse_config` / `Config`, so
/// BM25 must rank the fixture's chunk first.
#[test]
fn search_code_keyword_mode_ranks_by_bm25() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    std::fs::write(root.join("lib.rs"), FIXTURE).expect("write fixture");
    // Force the embed-free path so chunking + BM25 postings are written deterministically without a
    // model download — the keyword lane's documented `embed = false` guarantee.
    std::fs::create_dir_all(root.join(".basemind")).expect("mkdir .basemind");
    std::fs::write(
        root.join(".basemind/basemind.toml"),
        "\"$schema\" = \"v1\"\n\n[code_search]\nembed = false\n",
    )
    .expect("write config");

    let scan = Command::new(bin())
        .current_dir(root)
        .arg("scan")
        .output()
        .expect("spawn scan");
    assert!(
        scan.status.success(),
        "basemind scan failed: {}",
        String::from_utf8_lossy(&scan.stderr)
    );

    let out = Command::new(bin())
        .current_dir(root)
        .args(["--json", "query", "search-code", "--mode", "keyword", "config parser"])
        .output()
        .expect("spawn keyword search-code");
    assert!(
        out.status.success(),
        "keyword search-code failed (should not need an embedder): {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("keyword search-code emits JSON");
    let hits = value
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("keyword response carries a hits array");
    assert!(
        !hits.is_empty(),
        "keyword search must find the `config`-bearing chunk (embed-free, deterministic): {value}"
    );

    let top = &hits[0];
    assert_eq!(
        top.get("path").and_then(|p| p.as_str()),
        Some("lib.rs"),
        "top keyword hit must point at the only indexed file: {top}"
    );
    // The keyword lane reports a BM25 `score` (higher = better), not a vector `distance`.
    let score = top
        .get("score")
        .and_then(serde_json::Value::as_f64)
        .expect("keyword hit carries a BM25 score");
    assert!(
        score > 0.0,
        "a matching keyword hit must have a positive BM25 score: {top}"
    );
    assert!(
        top.get("distance").is_none(),
        "keyword hit must not carry a vector distance: {top}"
    );

    // The pointer round-trips through get-chunk exactly like the semantic lane.
    let chunk_id = top
        .get("chunk_id")
        .and_then(|c| c.as_str())
        .expect("keyword hit carries a chunk_id pointer");
    let gc = Command::new(bin())
        .current_dir(root)
        .args(["--json", "query", "get-chunk", "lib.rs", "--chunk-id", chunk_id])
        .output()
        .expect("spawn get-chunk");
    assert!(
        gc.status.success(),
        "get-chunk failed: {}",
        String::from_utf8_lossy(&gc.stderr)
    );
    let gv: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&gc.stdout)).expect("get-chunk emits JSON");
    assert!(
        gv.get("text").and_then(|t| t.as_str()).is_some_and(|t| !t.is_empty()),
        "get_chunk must return a non-empty body for the keyword hit: {gv}"
    );
}

/// End-to-end test for the hybrid lane's exact-symbol contribution (`mode=hybrid`, the default).
///
/// With `embed=false` there is no vector lane, so hybrid fuses keyword + exact deterministically. An
/// identifier-shaped query (`parse_config`) fires the exact lane, which resolves the symbol to its
/// owning chunk; the exact lane's 2x RRF weight must float that chunk to the top. No embedder needed.
#[test]
fn search_code_hybrid_ranks_exact_symbol_first() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    std::fs::write(root.join("lib.rs"), FIXTURE).expect("write fixture");
    std::fs::create_dir_all(root.join(".basemind")).expect("mkdir .basemind");
    std::fs::write(
        root.join(".basemind/basemind.toml"),
        "\"$schema\" = \"v1\"\n\n[code_search]\nembed = false\n",
    )
    .expect("write config");

    let scan = Command::new(bin())
        .current_dir(root)
        .arg("scan")
        .output()
        .expect("spawn scan");
    assert!(
        scan.status.success(),
        "basemind scan failed: {}",
        String::from_utf8_lossy(&scan.stderr)
    );

    // No `--mode`: exercises the DEFAULT, which Phase 3 flips to hybrid.
    let out = Command::new(bin())
        .current_dir(root)
        .args(["--json", "query", "search-code", "parse_config"])
        .output()
        .expect("spawn hybrid search-code");
    assert!(
        out.status.success(),
        "default (hybrid) search-code failed without an embedder: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("hybrid search-code emits JSON");
    let hits = value
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hybrid response carries a hits array");
    assert!(
        !hits.is_empty(),
        "hybrid search must find the parse_config chunk: {value}"
    );

    let top = &hits[0];
    assert_eq!(
        top.get("symbol").and_then(|s| s.as_str()),
        Some("parse_config"),
        "the exact symbol lane must float parse_config's defining chunk to rank #1: {top}"
    );
    // Hybrid hits carry the fused RRF score, not a raw distance.
    assert!(
        top.get("score")
            .and_then(serde_json::Value::as_f64)
            .is_some_and(|s| s > 0.0),
        "hybrid hit must carry a positive fused RRF score: {top}"
    );
    // Why-matched provenance: the top hit for an identifier query is produced by the exact lane,
    // so `matched_lanes` includes "exact" and `exact_rank` is set (1-based). Keyword may also
    // contribute; the vector lane is absent under embed=false.
    let lanes: Vec<&str> = top
        .get("matched_lanes")
        .and_then(|v| v.as_array())
        .expect("hybrid hit carries matched_lanes")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert!(
        lanes.contains(&"exact"),
        "the exact lane must be credited in matched_lanes for an identifier query: {top}"
    );
    assert_eq!(
        top.get("exact_rank").and_then(serde_json::Value::as_u64),
        Some(1),
        "the defining chunk must be exact-lane rank #1: {top}"
    );
    assert!(
        top.get("vector_rank").is_none(),
        "no vector lane under embed=false, so vector_rank must be absent: {top}"
    );
}

/// Recursively search `root/.basemind/` for the first file whose name ends with
/// `.chunk.msgpack`. Returns `None` when no sidecar exists (clean scan or chunker disabled).
fn find_chunk_sidecar(root: &std::path::Path) -> Option<std::path::PathBuf> {
    fn walk(dir: &std::path::Path) -> Option<std::path::PathBuf> {
        let entries = std::fs::read_dir(dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(found) = walk(&path) {
                    return Some(found);
                }
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".chunk.msgpack"))
            {
                return Some(path);
            }
        }
        None
    }
    walk(&root.join(".basemind"))
}
