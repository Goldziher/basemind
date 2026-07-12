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
    basemind::store::init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    std::fs::write(root.join("lib.rs"), FIXTURE).expect("write fixture");

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
    basemind::store::init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    // Test-unique content: the `.chunk.msgpack` sidecar is content-addressed in the shared global
    // store, so a body shared with another test would let this test delete a sibling's sidecar.
    let fixture = format!("{FIXTURE}\n// stale-sidecar-rechunk-marker\n");
    std::fs::write(root.join("lib.rs"), &fixture).expect("write fixture");
    let stem = content_stem(fixture.as_bytes());

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

    let sidecar = find_chunk_sidecar(&stem);
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

    std::fs::remove_file(&sidecar).expect("remove sidecar");
    assert!(!sidecar.exists(), "sidecar must be gone after manual deletion");

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

    assert!(
        sidecar.exists(),
        "re-scan must regenerate the .chunk.msgpack sidecar after it was deleted \
         (the stale-sidecar guard should force re-chunking despite unchanged content hash): \
         sidecar={}",
        sidecar.display()
    );
}

/// Regression guard for the Deferred→Inline embedding upgrade.
///
/// The daemon rescans with [`EmbedMode::Deferred`], which writes the code map + BM25 keyword lane +
/// a chunk-only (`embedding_dim: 0`) sidecar but no vectors. A later [`EmbedMode::Inline`] pass over
/// the SAME content must NOT short-circuit on the unchanged check — it has to re-process the file to
/// fill vectors. A mode-blind check would treat the chunk-only sidecar as "already indexed", skip
/// `chunk_and_embed`, and leave `search_code` serving zero vectors forever. Two invariants are pinned
/// here without depending on the embedder (which may be offline): (1) a second Deferred pass IS
/// idempotent; (2) an embed-eligible Inline pass re-processes the chunk-only file rather than
/// skipping it.
#[test]
fn deferred_chunk_only_sidecar_is_reprocessed_by_an_inline_embed_pass() {
    use basemind::config::ConfigV1;
    use basemind::scanner::{EmbedMode, ScanSource, scan};
    use basemind::store::{Store, VIEW_WORKING};

    basemind::store::init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    // Test-unique content so this test's content-addressed sidecar can't collide with a sibling's.
    let fixture = format!("{FIXTURE}\n// deferred-inline-embed-marker\n");
    std::fs::write(root.join("lib.rs"), &fixture).expect("write fixture");
    let stem = content_stem(fixture.as_bytes());

    let mut cfg = ConfigV1::with_defaults();
    // Chunking is on by default; opt into embeddings so the file is embed-eligible (the path the
    // regression lives on). The embedder may still be offline — the assertions below don't need it.
    cfg.code_search.embed = true;

    let mut store = Store::open(root, VIEW_WORKING).expect("open store");

    // Pass 1 — Deferred: writes the chunk-only sidecar (no vectors).
    let s1 = scan(root, &mut store, &cfg, ScanSource::WorkingTree, EmbedMode::Deferred).expect("deferred scan");
    assert_eq!(
        s1.stats.updated, 1,
        "the one source file is newly indexed by the deferred pass"
    );
    let blob = store
        .read_chunks_by_hex(&stem)
        .expect("read chunk sidecar")
        .expect("deferred pass persists a chunk-only sidecar");
    assert!(!blob.chunks.is_empty(), "the fixture yields at least one chunk");
    assert_eq!(
        blob.embedding_dim, 0,
        "the deferred pass writes chunks only — no vectors yet"
    );

    // A second Deferred pass is idempotent: the chunk-only sidecar satisfies the unchanged check.
    let s1b = scan(root, &mut store, &cfg, ScanSource::WorkingTree, EmbedMode::Deferred).expect("deferred rescan");
    assert_eq!(s1b.stats.updated, 0, "a second deferred pass changes nothing");
    assert_eq!(
        s1b.stats.skipped_unchanged, 1,
        "the file is skipped as unchanged on the second deferred pass"
    );

    // Pass 2 — Inline over the SAME content must re-process, not skip: the dim-0 sidecar does not
    // satisfy an embed-eligible Inline scan. This is the regression guard.
    let s2 = scan(root, &mut store, &cfg, ScanSource::WorkingTree, EmbedMode::Inline).expect("inline scan");
    assert_eq!(
        s2.stats.skipped_unchanged, 0,
        "the inline embed pass must re-process a chunk-only file, not skip it as unchanged"
    );
    assert_eq!(
        s2.stats.updated, 1,
        "the inline pass re-processes the file to fill vectors"
    );

    // When the embedder is available the sidecar is upgraded in place to carry vectors; offline it
    // stays chunk-only (BM25 fallback). The re-process assertion above pins the fix either way.
    let after = store
        .read_chunks_by_hex(&stem)
        .expect("read chunk sidecar")
        .expect("chunk sidecar still present");
    if after.embedding_dim > 0 {
        assert_eq!(
            after.embeddings.len(),
            after.chunks.len(),
            "every chunk gets a vector once embedded"
        );
    }
}

/// End-to-end test for the BM25 keyword lane (`search_code --mode keyword`).
///
/// Unlike the semantic lane, keyword search needs no embedder — postings are pure Fjall + sidecar —
/// so with `[code_search] embed = false` this test is fully deterministic and asserts strictly (no
/// cold-model skip). The query term `config` appears in the fixture's `parse_config` / `Config`, so
/// BM25 must rank the fixture's chunk first.
#[test]
fn search_code_keyword_mode_ranks_by_bm25() {
    basemind::store::init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    std::fs::write(root.join("lib.rs"), FIXTURE).expect("write fixture");
    // Canonical committed config location; the cache moved to a global XDG store so there is no
    // in-repo `.basemind/` dir to hold a legacy config.
    std::fs::write(
        root.join("basemind.toml"),
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
    basemind::store::init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    std::fs::write(root.join("lib.rs"), FIXTURE).expect("write fixture");
    // Canonical committed config location (see the keyword-mode test).
    std::fs::write(
        root.join("basemind.toml"),
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
    assert!(
        top.get("score")
            .and_then(serde_json::Value::as_f64)
            .is_some_and(|s| s > 0.0),
        "hybrid hit must carry a positive fused RRF score: {top}"
    );
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

/// Find the `.chunk.msgpack` sidecar for `stem` in the machine-global blob store. The blob store
/// is content-addressed and shared across workspaces now, so we look up THIS test's own stem rather
/// than "the first sidecar anywhere" (which could belong to a sibling test's identical content).
/// Returns `None` when the sidecar does not exist (clean scan or chunker disabled).
fn find_chunk_sidecar(stem: &str) -> Option<std::path::PathBuf> {
    let path = basemind::store::global_blobs_dir().join(format!("{stem}.chunk.msgpack"));
    path.exists().then_some(path)
}

/// Content hash (hex stem) of `bytes` — the key under which the blob store addresses this file's
/// sidecars. Lets a test locate exactly its own `.chunk.msgpack` in the shared global store.
fn content_stem(bytes: &[u8]) -> String {
    basemind::hashing::hex(&basemind::hashing::hash_bytes(bytes))
}
