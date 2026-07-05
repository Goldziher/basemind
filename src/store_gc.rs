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
//! ## Why a single content hash addresses both blob suffixes
//!
//! Each [`crate::store::FileEntry`] carries exactly one `hash_hex` — the content hash of the
//! source file. The scanner writes up to two blobs for that file, both keyed by the *same*
//! hash with different suffixes: `<hash>.fm.msgpack` (the combined L1 + L2 filemap) and
//! (documents build) `<hash>.doc.msgpack`. So the set of live blob stems is exactly the set
//! of `hash_hex` values across all entries of all views — there is no separate `fm_hash` /
//! `doc_hash` to union.

use std::path::Path;

use ahash::AHashSet;
use serde::Serialize;
use thiserror::Error;

use crate::store::{BLOBS_DIR, INDEX_FILE, StoreError, VIEWS_DIR, acquire_lock, read_index, wipe_blobs};

/// The blob filename suffixes the scanner emits today, all keyed by one content hash.
/// Used to strip the suffix off a blob filename to recover its hex stem. The four suffixes are
/// `.fm.msgpack` (combined L1 + L2 filemap), `.doc.msgpack` (documents tier), `.chunk.msgpack`
/// (code-search tier), and `.rref.msgpack` (code-intel resolved-references tier). All share the
/// same source-hash stem as the `.fm` blob, so they are reclaimed together when the source file
/// changes or is deleted (its stem drops out of the live set).
const BLOB_SUFFIXES: [&str; 4] = [".fm.msgpack", ".doc.msgpack", ".chunk.msgpack", ".rref.msgpack"];

/// Pre-0.9 split-tier blob suffixes (`<hash>.l1.msgpack` / `<hash>.l2.msgpack`), superseded by
/// the combined `.fm.msgpack` frame. No current code writes or reads these, so any left on disk
/// after a schema-bump refresh are dead format — the sweep deletes them on sight regardless of
/// whether their stem is still referenced (the live `.fm` blob shares that stem).
const LEGACY_BLOB_SUFFIXES: [&str; 2] = [".l1.msgpack", ".l2.msgpack"];

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
    /// Recursive byte size of the **on-disk** git cache (`git-cache/`). The git cache is a
    /// two-layer cache (RAM LRU + optional disk); this counts only the disk layer. A `0`
    /// therefore means nothing has been *persisted* — either no disk-backed git tool has run
    /// yet, or the server was started with `--no-git-cache-disk` (RAM-only by design), in
    /// which case live git-tool results are cached in RAM and legitimately leave no disk
    /// footprint. It is not, on its own, evidence that the git cache is unused.
    pub git_cache_bytes: u64,
    /// Byte size of `telemetry.jsonl`.
    pub telemetry_bytes: u64,
    /// Recursive byte size of the precomputed git-history index (`git-history.fjall/`). Added in
    /// 0.16: before that this directory (a sibling of `views/`, often hundreds of MB on a
    /// deep-history repo) was omitted, so the reported total undercounted `du` — the bug this
    /// field fixes.
    pub git_history_bytes: u64,
    /// Recursive byte size of the **entire** `.basemind/` tree. This is the ground-truth total
    /// (it matches `du`); the per-component fields are a breakdown of it, and any bytes not
    /// attributed to a named component land in [`Self::other_bytes`]. Computed from the whole
    /// tree so a future uncounted directory can never silently shrink the reported footprint.
    pub total_bytes: u64,
    /// Bytes under `.basemind/` not attributed to any named component (`total_bytes` minus the
    /// sum of the component fields): the legacy top-level `index.msgpack`, lock/id/config
    /// sidecars, `.gitignore`, and anything a future version adds before it gets its own field.
    pub other_bytes: u64,
    /// Total blob files on disk (every suffix counts as one file).
    pub blob_count: usize,
    /// Blob files whose hex stem is referenced by no view — reclaimable by [`run_gc`]. Meaningful
    /// only when [`Self::blob_accounting_ok`] is `true`; `0` otherwise (not computed).
    pub orphan_blob_count: usize,
    /// Whether orphan accounting ran. `false` means a view index couldn't be read (stale schema
    /// or corruption), so [`Self::orphan_blob_count`] is `0` because it was skipped, NOT because
    /// there are no orphans — the size fields are still accurate. Re-scan to restore accounting.
    pub blob_accounting_ok: bool,
    /// Per-view indexed file count, keyed by view name. Empty entries are still listed.
    pub per_view_file_count: Vec<(String, usize)>,
    /// Current resident set size (physical RAM) of the process answering this call, in bytes.
    /// `None` when unreadable on this platform. Inside `basemind serve` this is the live MCP
    /// server; from the one-shot CLI it is that transient process. See [`crate::sysres`].
    pub rss_bytes: Option<u64>,
    /// Peak resident set size of the reporting process over its lifetime, in bytes; `None` when
    /// unreadable. See [`crate::sysres`].
    pub peak_rss_bytes: Option<u64>,
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
        // Pre-0.9 split-tier blobs are dead format — reclaim unconditionally (their stem may
        // still be referenced by the live combined `.fm` blob, so the stem check below would
        // wrongly keep them).
        let is_legacy = LEGACY_BLOB_SUFFIXES.iter().any(|suffix| file_name.ends_with(suffix));
        let Some(stem) = blob_stem(file_name) else {
            report.scanned += 1;
            if is_legacy {
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                std::fs::remove_file(&path).map_err(|source| GcError::Io {
                    path: path.clone(),
                    source,
                })?;
                report.removed += 1;
                report.bytes_freed += size;
            }
            // Otherwise not a recognized blob (e.g. a `.tmp` writer leftover) — never delete.
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
        CacheComponent::GitCache => remove_dir_if_exists(&basemind_dir.join(crate::git_cache::GIT_CACHE_DIR))?,
        CacheComponent::Telemetry => remove_file_if_exists(&basemind_dir.join(TELEMETRY_FILENAME))?,
        CacheComponent::All => remove_dir_if_exists(basemind_dir)?,
    }
    Ok(())
}

/// Clear a single view by name: removes only `views/<name>/` (its `index.msgpack` + Fjall
/// trees), leaving every other view and the shared blob store intact. This is the targeted
/// counterpart to [`clear_component`]`(CacheComponent::Views)`, which removes the whole
/// `views/` directory.
///
/// The blobs a view referenced are NOT touched here — they are content-addressed and may be
/// shared with other views. Run [`run_gc`] afterwards to reclaim any now-orphaned blobs.
///
/// `name` is validated to be a single path component (no separators, no `..`) so a caller
/// can never escape the `views/` directory. Returns `Ok(())` even when the view does not
/// exist (idempotent), but errors on an invalid name.
pub fn clear_single_view(basemind_dir: &Path, name: &str) -> Result<(), GcError> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return Err(GcError::Io {
            path: basemind_dir.join(VIEWS_DIR).join(name),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid view name {name:?}: must be a single path component"),
            ),
        });
    }
    remove_dir_if_exists(&basemind_dir.join(VIEWS_DIR).join(name))
}

/// Gather per-component sizes and blob accounting without mutating anything. The orphan
/// count reuses [`collect_referenced_hashes`] but never deletes.
pub fn cache_stats(basemind_dir: &Path) -> Result<CacheStats, GcError> {
    let blobs_dir = basemind_dir.join(BLOBS_DIR);
    // cache_stats is read-only and never deletes, so — unlike `cache_gc` — it must NOT hard-fail
    // when a view index can't be read (e.g. a schema-mismatched `.basemind/` from an older binary
    // that hasn't been re-scanned). The disk/RAM footprint is exactly what an operator asking
    // "how much does this consume" needs, and it requires no index. So degrade: report sizes
    // regardless, and mark orphan accounting unavailable when the live set can't be determined.
    let referenced = match collect_referenced_hashes(basemind_dir) {
        Ok(set) => Some(set),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "cache_stats: could not read a view index (stale schema or corrupt); \
                 reporting sizes only, orphan accounting skipped"
            );
            None
        }
    };
    let blob_accounting_ok = referenced.is_some();

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
            let Some(stem) = path.file_name().and_then(|n| n.to_str()).and_then(blob_stem) else {
                continue;
            };
            blob_count += 1;
            // Only classify orphans when the live reference set is known; otherwise leave the
            // count at 0 and let `blob_accounting_ok = false` disclose it wasn't computed.
            if let Some(referenced) = &referenced
                && !referenced.contains(stem)
            {
                orphan_blob_count += 1;
            }
        }
    }

    let blobs_bytes = dir_size(&blobs_dir)?;
    let views_bytes = dir_size(&basemind_dir.join(VIEWS_DIR))?;
    let lance_bytes = dir_size(&basemind_dir.join("lance"))?;
    let git_cache_bytes = dir_size(&basemind_dir.join(crate::git_cache::GIT_CACHE_DIR))?;
    let telemetry_bytes = file_size(&basemind_dir.join(TELEMETRY_FILENAME))?;
    let git_history_bytes = dir_size(&basemind_dir.join(crate::git_history::GIT_HISTORY_DIR))?;

    // Ground-truth footprint: size the whole tree, then derive `other` as the remainder so the
    // breakdown always reconciles to the total and no directory can go uncounted.
    let total_bytes = dir_size(basemind_dir)?;
    let accounted = blobs_bytes + views_bytes + lance_bytes + git_cache_bytes + telemetry_bytes + git_history_bytes;
    let other_bytes = total_bytes.saturating_sub(accounted);

    let rss = crate::sysres::sample();

    Ok(CacheStats {
        blobs_bytes,
        views_bytes,
        lance_bytes,
        git_cache_bytes,
        telemetry_bytes,
        git_history_bytes,
        total_bytes,
        other_bytes,
        blob_count,
        orphan_blob_count,
        blob_accounting_ok,
        per_view_file_count: per_view_file_count(basemind_dir)?,
        rss_bytes: rss.current_bytes,
        peak_rss_bytes: rss.peak_bytes,
    })
}

// ─── internal helpers ───────────────────────────────────────────────────────

/// Strip a known blob suffix off a filename, returning the hex stem. `None` if the filename
/// is not a recognized blob (so the caller never treats stray files as reclaimable).
fn blob_stem(file_name: &str) -> Option<&str> {
    BLOB_SUFFIXES.iter().find_map(|suffix| file_name.strip_suffix(suffix))
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
        let name = view_dir.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
        let count = read_index(&view_dir).ok().flatten().map_or(0, |idx| idx.files.len());
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

/// On-disk (allocated) size of a filesystem entry, matching what `du` reports.
///
/// Uses the allocated block count (`blocks × 512`) on Unix rather than the apparent length
/// (`metadata().len()`). This matters because Fjall keeps **sparse** journal files: their apparent
/// length can be tens of MB while only a few hundred KB of blocks are actually allocated. Summing
/// apparent lengths over-reported the footprint many-fold (e.g. a 9.6 MB `.basemind/` read as
/// ~132 MB); block size is the ground truth that reconciles to `du`. On non-Unix we fall back to
/// the apparent length (no portable block API).
#[cfg(unix)]
fn on_disk_size(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
fn on_disk_size(meta: &std::fs::Metadata) -> u64 {
    meta.len()
}

/// On-disk size of a single file, or `0` if it is absent.
fn file_size(path: &Path) -> Result<u64, GcError> {
    if !path.exists() {
        return Ok(0);
    }
    let meta = std::fs::symlink_metadata(path).map_err(|source| GcError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(on_disk_size(&meta))
}

/// Recursive on-disk size of a directory tree, matching `du`: counts the allocated blocks of every
/// entry — the directory's own inode blocks included — via [`on_disk_size`]. Returns `0` for a
/// missing directory; follows no symlinks (counts the link entry itself, like `du` without `-L`).
fn dir_size(dir: &Path) -> Result<u64, GcError> {
    if !dir.exists() {
        return Ok(0);
    }
    // The directory's own allocation (`du` counts it too).
    let mut total = std::fs::symlink_metadata(dir).map(|m| on_disk_size(&m)).unwrap_or(0);
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
            // Recursion counts the subdirectory's own blocks + its contents.
            total += dir_size(&path)?;
        } else {
            total += on_disk_size(&meta);
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

        // Referenced blob: one combined filemap for the live stem.
        fs::write(blobs.join(format!("{referenced_stem}.fm.msgpack")), b"fm").expect("write ref fm");
        // Orphan blob: a single filemap with a known byte length.
        let orphan_bytes = b"orphan-blob-bytes";
        let orphan_len = orphan_bytes.len() as u64;
        fs::write(blobs.join(format!("{orphan_stem}.fm.msgpack")), orphan_bytes).expect("write orphan");

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
    fn cache_stats_counts_git_history_and_reconciles_total() {
        // Regression for the 0.15 disk-undercount bug: `git-history.fjall/` (a sibling of
        // `views/`) was never summed, so the reported total fell short of `du`. Assert it is now
        // counted and that the breakdown reconciles exactly to the whole-tree total.
        let fx = build_fixture();

        // A git-history index directory with a known payload …
        let gh_dir = fx.basemind_dir.join(crate::git_history::GIT_HISTORY_DIR);
        fs::create_dir_all(&gh_dir).expect("mk git-history");
        let gh_payload = b"git-history-index-bytes-XXXXXXXX";
        fs::write(gh_dir.join("commits.fjall"), gh_payload).expect("write gh blob");

        // … and a stray top-level file that belongs to no named component (→ `other_bytes`).
        let stray = b"lockmeta";
        fs::write(fx.basemind_dir.join(".lock.meta"), stray).expect("write stray");

        let stats = cache_stats(&fx.basemind_dir).expect("cache_stats");

        // Block-rounded (allocated) sizing, so assert coverage rather than exact byte counts: the
        // git-history directory is now counted (was silently omitted before 0.16).
        assert!(
            stats.git_history_bytes >= gh_payload.len() as u64,
            "git-history.fjall/ must be counted (got {})",
            stats.git_history_bytes
        );
        assert!(
            stats.other_bytes >= stray.len() as u64,
            "the unattributed stray file lands in other_bytes (got {})",
            stats.other_bytes
        );

        // The whole-tree total is ground truth, and the breakdown reconciles to it exactly.
        assert_eq!(
            stats.total_bytes,
            dir_size(&fx.basemind_dir).expect("dir_size"),
            "total_bytes is the whole .basemind/ tree"
        );
        let component_sum = stats.blobs_bytes
            + stats.views_bytes
            + stats.lance_bytes
            + stats.git_cache_bytes
            + stats.telemetry_bytes
            + stats.git_history_bytes;
        assert_eq!(
            stats.total_bytes,
            component_sum + stats.other_bytes,
            "components + other must reconcile to total"
        );
    }

    #[test]
    fn cache_stats_degrades_when_index_unreadable() {
        // A corrupt/unreadable view index (e.g. an older-schema `.basemind/` not yet re-scanned)
        // must not sink the whole call: sizes are still reported, orphan accounting is skipped,
        // and `blob_accounting_ok` discloses that. (GC, which deletes, still hard-fails — asserted
        // separately by the collect/gc tests.)
        let fx = build_fixture();
        let working = fx.basemind_dir.join(VIEWS_DIR).join("working");
        // Overwrite the valid index with bytes rmp-serde can't decode.
        fs::write(working.join(INDEX_FILE), b"\xff\xff not-msgpack \x00").expect("corrupt index");

        // collect_referenced_hashes (the GC safety path) still errors …
        assert!(
            collect_referenced_hashes(&fx.basemind_dir).is_err(),
            "an unreadable index must fail the delete-path safety check"
        );

        // … but read-only stats degrade gracefully.
        let stats = cache_stats(&fx.basemind_dir).expect("cache_stats must not hard-fail");
        assert!(
            !stats.blob_accounting_ok,
            "orphan accounting must be flagged unavailable"
        );
        assert_eq!(
            stats.orphan_blob_count, 0,
            "orphan count is 0 (skipped), not a real zero"
        );
        assert!(stats.blob_count >= 2, "blob files are still counted by size walk");
        assert!(stats.total_bytes > 0, "sizes are still reported");
        assert_eq!(
            stats.total_bytes,
            dir_size(&fx.basemind_dir).expect("dir_size"),
            "total still reconciles to the tree"
        );
    }

    #[test]
    fn should_collect_only_referenced_stem() {
        let fx = build_fixture();
        let referenced = collect_referenced_hashes(&fx.basemind_dir).expect("collect");
        assert_eq!(referenced.len(), 1, "exactly one live stem");
        assert!(referenced.contains(&fx.referenced_stem), "live stem present");
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

        assert_eq!(report.scanned, 2, "one ref blob + one orphan inspected");
        assert_eq!(report.removed, 1, "only the orphan removed");
        assert_eq!(
            report.bytes_freed, fx.orphan_len,
            "freed bytes equal the orphan's exact length"
        );

        let blobs = fx.basemind_dir.join(BLOBS_DIR);
        assert!(
            blobs.join(format!("{}.fm.msgpack", fx.referenced_stem)).exists(),
            "referenced filemap survives"
        );
        assert!(
            !blobs.join(format!("{}.fm.msgpack", fx.orphan_stem)).exists(),
            "orphan filemap gone"
        );
    }

    #[test]
    fn should_reclaim_legacy_split_tier_blobs_even_when_stem_is_referenced() {
        // A pre-0.9 `.l1`/`.l2` pair left on disk after the schema-bump refresh shares its stem
        // with the live combined `.fm` blob — the stem IS referenced, yet the dead-format pair
        // must still be reaped.
        let fx = build_fixture();
        let blobs = fx.basemind_dir.join(BLOBS_DIR);
        fs::write(blobs.join(format!("{}.l1.msgpack", fx.referenced_stem)), b"legacy-l1").expect("write legacy l1");
        fs::write(blobs.join(format!("{}.l2.msgpack", fx.referenced_stem)), b"legacy-l2").expect("write legacy l2");

        let referenced = collect_referenced_hashes(&fx.basemind_dir).expect("collect");
        assert!(
            referenced.contains(&fx.referenced_stem),
            "stem is referenced by the live index"
        );
        let report = gc_blobs(&fx.basemind_dir, &referenced).expect("gc");

        assert_eq!(report.removed, 3, "two legacy split blobs + the orphan filemap");
        assert!(
            !blobs.join(format!("{}.l1.msgpack", fx.referenced_stem)).exists(),
            "legacy l1 reclaimed despite a referenced stem"
        );
        assert!(
            !blobs.join(format!("{}.l2.msgpack", fx.referenced_stem)).exists(),
            "legacy l2 reclaimed despite a referenced stem"
        );
        assert!(
            blobs.join(format!("{}.fm.msgpack", fx.referenced_stem)).exists(),
            "the live combined filemap survives"
        );
    }

    #[test]
    fn should_report_one_orphan_before_gc_and_zero_after() {
        let fx = build_fixture();

        let before = cache_stats(&fx.basemind_dir).expect("stats before");
        assert_eq!(before.blob_count, 2, "two blob files on disk");
        assert_eq!(before.orphan_blob_count, 1, "one orphan before GC");
        assert_eq!(
            before.per_view_file_count,
            vec![("working".to_string(), 1)],
            "single working view with one indexed file"
        );

        run_gc(&fx.basemind_dir).expect("gc");

        let after = cache_stats(&fx.basemind_dir).expect("stats after");
        assert_eq!(after.blob_count, 1, "orphan reaped");
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

    /// Build a fixture with two scanned views (`working` + `rev-abc`), each with a real
    /// `index.msgpack`, sharing the blob store. Returns the basemind dir.
    fn build_two_view_fixture() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let basemind_dir = tmp.path().join(".basemind");
        for view in ["working", "rev-abc"] {
            let view_dir = basemind_dir.join(VIEWS_DIR).join(view);
            fs::create_dir_all(&view_dir).expect("mk view");
            let mut index = Index::empty();
            index.files.insert(
                crate::path::RelPath::from("src/main.rs"),
                FileEntry {
                    hash_hex: "a".repeat(64),
                    language: "rust".to_string(),
                    size_bytes: 2,
                    mtime: 0,
                },
            );
            let bytes = rmp_serde::to_vec_named(&index).expect("encode");
            fs::write(view_dir.join(INDEX_FILE), bytes).expect("write index");
        }
        (tmp, basemind_dir)
    }

    #[test]
    fn should_clear_single_view_and_leave_others_intact() {
        // bug #22: clearing one view by name must NOT nuke every view.
        let (_tmp, basemind_dir) = build_two_view_fixture();

        clear_single_view(&basemind_dir, "rev-abc").expect("clear one view");

        assert!(
            !basemind_dir.join(VIEWS_DIR).join("rev-abc").exists(),
            "named view removed"
        );
        assert!(
            basemind_dir.join(VIEWS_DIR).join("working").join(INDEX_FILE).exists(),
            "other view survives single-view clear"
        );
    }

    #[test]
    fn clear_single_view_is_idempotent_for_missing_view() {
        let (_tmp, basemind_dir) = build_two_view_fixture();
        clear_single_view(&basemind_dir, "rev-does-not-exist").expect("missing view is a no-op");
        // Existing views untouched.
        assert!(basemind_dir.join(VIEWS_DIR).join("working").exists());
        assert!(basemind_dir.join(VIEWS_DIR).join("rev-abc").exists());
    }

    #[test]
    fn clear_single_view_rejects_path_traversal() {
        let (_tmp, basemind_dir) = build_two_view_fixture();
        for bad in ["..", "a/b", "../escape", ""] {
            assert!(
                clear_single_view(&basemind_dir, bad).is_err(),
                "invalid view name {bad:?} must be rejected"
            );
        }
        // The legitimate views are untouched by the rejected calls.
        assert!(basemind_dir.join(VIEWS_DIR).join("working").exists());
    }

    #[test]
    fn blob_stem_recovers_stem_for_every_known_suffix() {
        // Every suffix in BLOB_SUFFIXES must strip back to the bare hex stem — a suffix missing
        // from the list makes gc_blobs take the "never delete" branch and leak that tier forever
        // (the .chunk / .rref regression this covers).
        assert_eq!(blob_stem("deadbeef.fm.msgpack"), Some("deadbeef"));
        assert_eq!(blob_stem("deadbeef.doc.msgpack"), Some("deadbeef"));
        assert_eq!(blob_stem("deadbeef.chunk.msgpack"), Some("deadbeef"));
        assert_eq!(blob_stem("deadbeef.rref.msgpack"), Some("deadbeef"));
        // A stray file that matches no known suffix is not a reclaimable blob.
        assert_eq!(blob_stem("deadbeef.tmp"), None);
    }

    #[test]
    fn should_reclaim_unreferenced_chunk_and_rref_but_keep_referenced() {
        // Regression: `.chunk.msgpack` (code-search) and `.rref.msgpack` (code-intel) blobs share
        // the source-hash stem with `.fm`. An orphan-stem chunk/rref must be reaped, while a
        // referenced-stem chunk/rref (still pointed at by the live index) must survive.
        let fx = build_fixture();
        let blobs = fx.basemind_dir.join(BLOBS_DIR);

        // Referenced-stem sidecar tiers: kept because the live index references the stem.
        fs::write(
            blobs.join(format!("{}.chunk.msgpack", fx.referenced_stem)),
            b"ref-chunk",
        )
        .expect("ref chunk");
        fs::write(blobs.join(format!("{}.rref.msgpack", fx.referenced_stem)), b"ref-rref").expect("ref rref");
        // Orphan-stem sidecar tiers: reclaimed because no view references the stem.
        fs::write(blobs.join(format!("{}.chunk.msgpack", fx.orphan_stem)), b"orphan-chunk").expect("orphan chunk");
        fs::write(blobs.join(format!("{}.rref.msgpack", fx.orphan_stem)), b"orphan-rref").expect("orphan rref");

        let referenced = collect_referenced_hashes(&fx.basemind_dir).expect("collect");
        gc_blobs(&fx.basemind_dir, &referenced).expect("gc");

        assert!(
            blobs.join(format!("{}.chunk.msgpack", fx.referenced_stem)).exists(),
            "referenced chunk survives"
        );
        assert!(
            blobs.join(format!("{}.rref.msgpack", fx.referenced_stem)).exists(),
            "referenced rref survives"
        );
        assert!(
            !blobs.join(format!("{}.chunk.msgpack", fx.orphan_stem)).exists(),
            "orphan chunk reclaimed"
        );
        assert!(
            !blobs.join(format!("{}.rref.msgpack", fx.orphan_stem)).exists(),
            "orphan rref reclaimed"
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
        assert!("nonsense".parse::<CacheComponent>().is_err(), "unknown token rejected");
    }
}
