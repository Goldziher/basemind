//! Cache garbage-collection + cleanup for the `.basemind/` directory.
//!
//! Two responsibilities:
//!
//! 1. **Mark-and-sweep GC of the shared blob store** ([`run_gc`]). Blobs under
//!    `.basemind/blobs/` are content-addressed and shared across every view; a blob is
//!    *live* iff some view's `index.msgpack` still references its content hash. Re-scans
//!    and branch switches leave behind blobs no view points at anymore — this reclaims them.
//! 2. **Whole-component cleanup** ([`clear_component`]) and **introspection**
//!    ([`cache_stats`]) for the CLI / MCP admin surface (wired up by separate workstreams).
//!
//! ## Why a single content hash addresses three blob suffixes
//!
//! Each [`crate::store::FileEntry`] carries exactly one `hash_hex` — the content hash of the
//! source file. The scanner writes up to three blobs for that file, all keyed by the *same*
//! hash with different suffixes: `<hash>.l1.msgpack`, `<hash>.l2.msgpack`, and (documents
//! build) `<hash>.doc.msgpack`. So the set of live blob stems is exactly the set of
//! `hash_hex` values across all entries of all views — there is no separate `l1_hash` /
//! `l2_hash` / `doc_hash` to union.

use std::path::Path;

use ahash::AHashSet;
use serde::Serialize;
use thiserror::Error;

use crate::store::{
    BLOBS_DIR, INDEX_FILE, StoreError, VIEWS_DIR, acquire_lock, read_index, wipe_blobs,
};

/// The three blob filename suffixes the scanner emits, all keyed by one content hash.
/// Used to strip the suffix off a blob filename to recover its hex stem.
const BLOB_SUFFIXES: [&str; 3] = [".l1.msgpack", ".l2.msgpack", ".doc.msgpack"];

/// Telemetry sink filename under `.basemind/`. Mirrors
/// [`crate::mcp::telemetry::TELEMETRY_FILENAME`]; duplicated here to avoid a dependency on
/// the MCP module from the cleanup layer.
const TELEMETRY_FILENAME: &str = "telemetry.jsonl";

/// Errors raised by the cache GC + cleanup layer. Wraps [`StoreError`] for the shared
/// blob/index machinery and adds a thin I/O variant for the directory walks this module
/// performs directly.
#[derive(Debug, Error)]
pub enum GcError {
    /// An underlying store operation failed (index read, blob wipe, lock acquisition).
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A filesystem operation in the GC walk failed, annotated with the offending path.
    #[error("io error on {path}: {source}")]
    Io {
        /// The path the failing operation targeted.
        path: std::path::PathBuf,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },
}

/// A clearable component of the `.basemind/` cache directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheComponent {
    /// Content-addressed extraction blobs under `blobs/`.
    Blobs,
    /// Per-view `index.msgpack` + Fjall index trees under `views/`.
    Views,
    /// LanceDB vector store under `lance/` (intelligence builds only).
    Lance,
    /// `gix`-backed history/blame cache under `git-cache/`.
    GitCache,
    /// MCP per-call telemetry log (`telemetry.jsonl`).
    Telemetry,
    /// Everything: the whole `.basemind/` directory.
    All,
}

impl CacheComponent {
    /// The canonical lowercase token for this component, matching its [`std::str::FromStr`].
    pub fn as_str(self) -> &'static str {
        match self {
            CacheComponent::Blobs => "blobs",
            CacheComponent::Views => "views",
            CacheComponent::Lance => "lance",
            CacheComponent::GitCache => "git-cache",
            CacheComponent::Telemetry => "telemetry",
            CacheComponent::All => "all",
        }
    }
}

impl std::str::FromStr for CacheComponent {
    type Err = String;

    /// Parse a component token. Accepts `blobs|views|lance|git-cache|telemetry|all`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "blobs" => Ok(CacheComponent::Blobs),
            "views" => Ok(CacheComponent::Views),
            "lance" => Ok(CacheComponent::Lance),
            "git-cache" => Ok(CacheComponent::GitCache),
            "telemetry" => Ok(CacheComponent::Telemetry),
            "all" => Ok(CacheComponent::All),
            other => Err(format!(
                "unknown cache component {other:?}; expected one of \
                 blobs|views|lance|git-cache|telemetry|all"
            )),
        }
    }
}

/// Result of a blob garbage-collection sweep.
#[derive(Debug, Clone, Serialize)]
pub struct GcReport {
    /// Total blob files inspected.
    pub scanned: usize,
    /// Orphan blob files removed.
    pub removed: usize,
    /// Bytes reclaimed by the removals (stat'd before deletion).
    pub bytes_freed: u64,
}

/// Per-component byte sizes + blob accounting for the `.basemind/` cache.
#[derive(Debug, Clone, Serialize)]
pub struct CacheStats {
    /// Recursive byte size of `blobs/`.
    pub blobs_bytes: u64,
    /// Recursive byte size of `views/`.
    pub views_bytes: u64,
    /// Recursive byte size of `lance/`.
    pub lance_bytes: u64,
    /// Recursive byte size of `git-cache/`.
    pub git_cache_bytes: u64,
    /// Byte size of `telemetry.jsonl`.
    pub telemetry_bytes: u64,
    /// Total blob files on disk (every suffix counts as one file).
    pub blob_count: usize,
    /// Blob files whose hex stem is referenced by no view — reclaimable by [`run_gc`].
    pub orphan_blob_count: usize,
    /// Per-view indexed file count, keyed by view name. Empty entries are still listed.
    pub per_view_file_count: Vec<(String, usize)>,
}

/// Enumerate every view's `index.msgpack` and union the hex content hashes it references.
///
/// A blob is live iff *any* view points at its content hash, so the union across all views
/// is the complete live set; the returned stems compare directly against on-disk blob
/// filenames (which are `<hex-stem>.{l1,l2,doc}.msgpack`).
///
/// ## Safety of the unreadable-view case
///
/// A view directory that simply has no `index.msgpack` yet (`read_index` returns
/// `Ok(None)`) contributes nothing and is skipped — it genuinely references no blobs.
///
/// Any *other* read failure (corrupt msgpack, schema mismatch, I/O error) is treated as a
/// hard error and propagated. Silently skipping such a view would drop its live hashes from
/// the union and cause the subsequent sweep to delete blobs that are in fact still
/// referenced — orphaning the entire store. Refusing to sweep when the live set might be
/// incomplete is the safe failure mode: the caller surfaces the error and the operator can
/// re-scan to rebuild the offending view's index before retrying GC.
pub fn collect_referenced_hashes(basemind_dir: &Path) -> Result<AHashSet<String>, GcError> {
    let mut referenced = AHashSet::new();
    let views_dir = basemind_dir.join(VIEWS_DIR);
    if !views_dir.exists() {
        return Ok(referenced);
    }
    for entry in read_dir(&views_dir)? {
        let entry = entry.map_err(|source| GcError::Io {
            path: views_dir.clone(),
            source,
        })?;
        let view_dir = entry.path();
        if !view_dir.is_dir() {
            continue;
        }
        // Cheap fast-path: a view with no index file references nothing. Skip without a read.
        if !view_dir.join(INDEX_FILE).exists() {
            tracing::warn!(view = %view_dir.display(), "view has no index.msgpack; skipping");
            continue;
        }
        // Propagate any non-missing read failure: an incomplete live set is unsafe to sweep.
        let index = match read_index(&view_dir) {
            Ok(Some(idx)) => idx,
            // Raced removal between the exists() check and the read — nothing to contribute.
            Ok(None) => continue,
            Err(e) => return Err(GcError::Store(e)),
        };
        for entry in index.files.values() {
            referenced.insert(entry.hash_hex.clone());
        }
    }
    Ok(referenced)
}

/// Sweep `blobs/`, deleting every blob whose hex stem is not in `referenced`.
///
/// Files that do not match a known blob suffix are inspected (counted in `scanned`) but
/// never deleted — a conservative choice so a stray file under `blobs/` is never reaped.
pub fn gc_blobs(basemind_dir: &Path, referenced: &AHashSet<String>) -> Result<GcReport, GcError> {
    let blobs_dir = basemind_dir.join(BLOBS_DIR);
    let mut report = GcReport {
        scanned: 0,
        removed: 0,
        bytes_freed: 0,
    };
    if !blobs_dir.exists() {
        return Ok(report);
    }
    for entry in read_dir(&blobs_dir)? {
        let entry = entry.map_err(|source| GcError::Io {
            path: blobs_dir.clone(),
            source,
        })?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(stem) = blob_stem(file_name) else {
            // Not a recognized blob (e.g. a `.tmp` writer leftover) — count but never delete.
            report.scanned += 1;
            continue;
        };
        report.scanned += 1;
        if referenced.contains(stem) {
            continue;
        }
        let size = std::fs::metadata(&path)
            .map_err(|source| GcError::Io {
                path: path.clone(),
                source,
            })?
            .len();
        std::fs::remove_file(&path).map_err(|source| GcError::Io {
            path: path.clone(),
            source,
        })?;
        report.removed += 1;
        report.bytes_freed += size;
    }
    Ok(report)
}

/// Race-safe blob GC: mark + sweep under the store's advisory lock.
///
/// The scanner writes a blob to disk *before* committing the index entry that references it
/// (see `write_blob` / `process_file` in `store.rs` / `scanner.rs`). Without the lock, a GC
/// running concurrently with a scan could observe the just-written-but-not-yet-referenced
/// blob, find no view pointing at it, and delete it out from under the scan. Holding the
/// exclusive `.lock` for the whole mark+sweep serializes against any concurrent
/// `basemind scan` / `basemind watch`, so every blob a scan has written is either already
/// referenced (committed) or invisible to GC (scan still holds the lock).
pub fn run_gc(basemind_dir: &Path) -> Result<GcReport, GcError> {
    // Held for the whole mark+sweep; dropped when `_lock` goes out of scope.
    let _lock = acquire_lock(basemind_dir)?;
    let referenced = collect_referenced_hashes(basemind_dir)?;
    gc_blobs(basemind_dir, &referenced)
}

/// Clear a whole cache component. Reuses the store's existing wipe helpers where they
/// exist; mirrors the lance dir-wipe pattern for the (feature-gated) vector store.
pub fn clear_component(basemind_dir: &Path, component: CacheComponent) -> Result<(), GcError> {
    match component {
        CacheComponent::Blobs => wipe_blobs(basemind_dir)?,
        CacheComponent::Views => remove_dir_if_exists(&basemind_dir.join(VIEWS_DIR))?,
        CacheComponent::Lance => clear_lance(basemind_dir)?,
        CacheComponent::GitCache => {
            remove_dir_if_exists(&basemind_dir.join(crate::git_cache::GIT_CACHE_DIR))?
        }
        CacheComponent::Telemetry => remove_file_if_exists(&basemind_dir.join(TELEMETRY_FILENAME))?,
        CacheComponent::All => remove_dir_if_exists(basemind_dir)?,
    }
    Ok(())
}

/// Gather per-component sizes and blob accounting without mutating anything. The orphan
/// count reuses [`collect_referenced_hashes`] but never deletes.
pub fn cache_stats(basemind_dir: &Path) -> Result<CacheStats, GcError> {
    let blobs_dir = basemind_dir.join(BLOBS_DIR);
    let referenced = collect_referenced_hashes(basemind_dir)?;

    let mut blob_count = 0usize;
    let mut orphan_blob_count = 0usize;
    if blobs_dir.exists() {
        for entry in read_dir(&blobs_dir)? {
            let entry = entry.map_err(|source| GcError::Io {
                path: blobs_dir.clone(),
                source,
            })?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(stem) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(blob_stem)
            else {
                continue;
            };
            blob_count += 1;
            if !referenced.contains(stem) {
                orphan_blob_count += 1;
            }
        }
    }

    Ok(CacheStats {
        blobs_bytes: dir_size(&blobs_dir)?,
        views_bytes: dir_size(&basemind_dir.join(VIEWS_DIR))?,
        lance_bytes: dir_size(&basemind_dir.join("lance"))?,
        git_cache_bytes: dir_size(&basemind_dir.join(crate::git_cache::GIT_CACHE_DIR))?,
        telemetry_bytes: file_size(&basemind_dir.join(TELEMETRY_FILENAME))?,
        blob_count,
        orphan_blob_count,
        per_view_file_count: per_view_file_count(basemind_dir)?,
    })
}

// ─── internal helpers ───────────────────────────────────────────────────────

/// Strip a known blob suffix off a filename, returning the hex stem. `None` if the filename
/// is not a recognized blob (so the caller never treats stray files as reclaimable).
fn blob_stem(file_name: &str) -> Option<&str> {
    BLOB_SUFFIXES
        .iter()
        .find_map(|suffix| file_name.strip_suffix(suffix))
}

/// Per-view indexed file count. A view whose index is missing or unreadable contributes a
/// `0` so the operator still sees the view listed.
fn per_view_file_count(basemind_dir: &Path) -> Result<Vec<(String, usize)>, GcError> {
    let mut out = Vec::new();
    let views_dir = basemind_dir.join(VIEWS_DIR);
    if !views_dir.exists() {
        return Ok(out);
    }
    for entry in read_dir(&views_dir)? {
        let entry = entry.map_err(|source| GcError::Io {
            path: views_dir.clone(),
            source,
        })?;
        let view_dir = entry.path();
        if !view_dir.is_dir() {
            continue;
        }
        let name = view_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        let count = read_index(&view_dir)
            .ok()
            .flatten()
            .map_or(0, |idx| idx.files.len());
        out.push((name, count));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Wipe every file/dir under `.basemind/lance/`, keeping the dir itself — mirrors the
/// `wipe_on_mismatch` pattern in `src/lance/mod.rs`. Feature-gated: the lance store only
/// exists in intelligence builds, so on a code-only build this is a no-op.
#[cfg(feature = "intelligence")]
fn clear_lance(basemind_dir: &Path) -> Result<(), GcError> {
    remove_dir_if_exists(&basemind_dir.join(crate::store::LANCE_DIR))
}

/// No-op on builds without the vector store compiled in.
#[cfg(not(feature = "intelligence"))]
fn clear_lance(_basemind_dir: &Path) -> Result<(), GcError> {
    Ok(())
}

fn remove_dir_if_exists(dir: &Path) -> Result<(), GcError> {
    if dir.exists() {
        std::fs::remove_dir_all(dir).map_err(|source| GcError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
    }
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<(), GcError> {
    if path.exists() {
        std::fs::remove_file(path).map_err(|source| GcError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    }
    Ok(())
}

fn read_dir(dir: &Path) -> Result<std::fs::ReadDir, GcError> {
    std::fs::read_dir(dir).map_err(|source| GcError::Io {
        path: dir.to_path_buf(),
        source,
    })
}

/// Byte size of a single file, or `0` if it is absent.
fn file_size(path: &Path) -> Result<u64, GcError> {
    if !path.exists() {
        return Ok(0);
    }
    Ok(std::fs::metadata(path)
        .map_err(|source| GcError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .len())
}

/// Recursive byte size of a directory tree. Returns `0` for a missing directory; follows no
/// symlinks (counts the link entry's own size only, like `du` without `-L`).
fn dir_size(dir: &Path) -> Result<u64, GcError> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut total = 0u64;
    for entry in read_dir(dir)? {
        let entry = entry.map_err(|source| GcError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let meta = entry.metadata().map_err(|source| GcError::Io {
            path: path.clone(),
            source,
        })?;
        if meta.is_dir() {
            total += dir_size(&path)?;
        } else {
            total += meta.len();
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{FileEntry, INDEX_FILE, Index};
    use std::fs;
    use std::path::PathBuf;

    /// A referenced + an orphan blob, with a hand-written `views/working/index.msgpack`
    /// pointing only at the referenced stem. Returns `(basemind_dir, referenced_stem,
    /// orphan_stem, orphan_byte_len)`.
    struct Fixture {
        _tmp: tempfile::TempDir,
        basemind_dir: PathBuf,
        referenced_stem: String,
        orphan_stem: String,
        orphan_len: u64,
    }

    fn build_fixture() -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let basemind_dir = tmp.path().join(".basemind");
        let blobs = basemind_dir.join(BLOBS_DIR);
        let working = basemind_dir.join(VIEWS_DIR).join("working");
        fs::create_dir_all(&blobs).expect("mk blobs");
        fs::create_dir_all(&working).expect("mk view");

        // 64-hex-char stems (matches the real hashing::hex output width).
        let referenced_stem = "a".repeat(64);
        let orphan_stem = "b".repeat(64);

        // Referenced blob: write l1 + l2 for the live stem.
        fs::write(blobs.join(format!("{referenced_stem}.l1.msgpack")), b"l1")
            .expect("write ref l1");
        fs::write(blobs.join(format!("{referenced_stem}.l2.msgpack")), b"l2-")
            .expect("write ref l2");
        // Orphan blob: a single l1 with a known byte length.
        let orphan_bytes = b"orphan-blob-bytes";
        let orphan_len = orphan_bytes.len() as u64;
        fs::write(
            blobs.join(format!("{orphan_stem}.l1.msgpack")),
            orphan_bytes,
        )
        .expect("write orphan");

        // Hand-build a real Index referencing only the live stem, serialized with the same
        // rmp-serde `to_vec_named` the store's `flush` uses.
        let mut index = Index::empty();
        index.files.insert(
            crate::path::RelPath::from("src/main.rs"),
            FileEntry {
                hash_hex: referenced_stem.clone(),
                language: "rust".to_string(),
                size_bytes: 2,
                mtime: 0,
            },
        );
        let bytes = rmp_serde::to_vec_named(&index).expect("encode index");
        fs::write(working.join(INDEX_FILE), bytes).expect("write index");

        Fixture {
            _tmp: tmp,
            basemind_dir,
            referenced_stem,
            orphan_stem,
            orphan_len,
        }
    }

    #[test]
    fn should_collect_only_referenced_stem() {
        let fx = build_fixture();
        let referenced = collect_referenced_hashes(&fx.basemind_dir).expect("collect");
        assert_eq!(referenced.len(), 1, "exactly one live stem");
        assert!(
            referenced.contains(&fx.referenced_stem),
            "live stem present"
        );
        assert!(
            !referenced.contains(&fx.orphan_stem),
            "orphan stem must not be referenced"
        );
    }

    #[test]
    fn should_remove_only_orphan_blob() {
        let fx = build_fixture();
        let referenced = collect_referenced_hashes(&fx.basemind_dir).expect("collect");
        let report = gc_blobs(&fx.basemind_dir, &referenced).expect("gc");

        assert_eq!(report.scanned, 3, "two ref blobs + one orphan inspected");
        assert_eq!(report.removed, 1, "only the orphan removed");
        assert_eq!(
            report.bytes_freed, fx.orphan_len,
            "freed bytes equal the orphan's exact length"
        );

        let blobs = fx.basemind_dir.join(BLOBS_DIR);
        assert!(
            blobs
                .join(format!("{}.l1.msgpack", fx.referenced_stem))
                .exists(),
            "referenced l1 survives"
        );
        assert!(
            blobs
                .join(format!("{}.l2.msgpack", fx.referenced_stem))
                .exists(),
            "referenced l2 survives"
        );
        assert!(
            !blobs
                .join(format!("{}.l1.msgpack", fx.orphan_stem))
                .exists(),
            "orphan l1 gone"
        );
    }

    #[test]
    fn should_report_one_orphan_before_gc_and_zero_after() {
        let fx = build_fixture();

        let before = cache_stats(&fx.basemind_dir).expect("stats before");
        assert_eq!(before.blob_count, 3, "three blob files on disk");
        assert_eq!(before.orphan_blob_count, 1, "one orphan before GC");
        assert_eq!(
            before.per_view_file_count,
            vec![("working".to_string(), 1)],
            "single working view with one indexed file"
        );

        run_gc(&fx.basemind_dir).expect("gc");

        let after = cache_stats(&fx.basemind_dir).expect("stats after");
        assert_eq!(after.blob_count, 2, "orphan reaped");
        assert_eq!(after.orphan_blob_count, 0, "no orphans remain");
    }

    #[test]
    fn should_clear_only_blobs_component() {
        let fx = build_fixture();
        // Drop a telemetry file so we can prove it survives the Blobs clear.
        fs::write(fx.basemind_dir.join(TELEMETRY_FILENAME), b"{}\n").expect("telemetry");

        clear_component(&fx.basemind_dir, CacheComponent::Blobs).expect("clear blobs");

        let blobs = fx.basemind_dir.join(BLOBS_DIR);
        let remaining: Vec<_> = fs::read_dir(&blobs)
            .expect("read blobs")
            .filter_map(Result::ok)
            .collect();
        assert!(remaining.is_empty(), "blobs dir emptied: {remaining:?}");
        assert!(blobs.exists(), "blobs dir itself preserved");

        // Other components untouched.
        assert!(
            fx.basemind_dir
                .join(VIEWS_DIR)
                .join("working")
                .join(INDEX_FILE)
                .exists(),
            "view index untouched by Blobs clear"
        );
        assert!(
            fx.basemind_dir.join(TELEMETRY_FILENAME).exists(),
            "telemetry untouched by Blobs clear"
        );
    }

    #[test]
    fn should_round_trip_component_tokens() {
        for component in [
            CacheComponent::Blobs,
            CacheComponent::Views,
            CacheComponent::Lance,
            CacheComponent::GitCache,
            CacheComponent::Telemetry,
            CacheComponent::All,
        ] {
            let token = component.as_str();
            let parsed: CacheComponent = token.parse().expect("parse token");
            assert_eq!(parsed, component, "round-trip {token}");
        }
        assert!(
            "nonsense".parse::<CacheComponent>().is_err(),
            "unknown token rejected"
        );
    }
}
