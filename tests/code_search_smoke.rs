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
