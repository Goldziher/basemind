//! Whole-component cleanup + introspection for the `.basemind/` cache — the CLI / MCP admin
//! surface ([`clear_component`], [`clear_single_view`], [`cache_stats`]) and the `du`-accurate
//! on-disk sizing it reports with.
//!
//! This is the second of the two responsibilities `store_gc.rs` used to carry (the first being
//! mark-and-sweep blob GC). Carved out because that file was over the 1000-line module cap and the
//! seam is real: nothing here reference-counts or reclaims a blob — it clears a named component
//! wholesale, or it only *measures*. It changes when the cache gains a component or a size field;
//! the sweep changes when the liveness rules change. `store_gc.rs` re-exports every public item, so
//! callers keep importing them from `crate::store_gc`.

use std::path::Path;

use serde::Serialize;

use crate::store::{VIEWS_DIR, global_blobs_dir, read_index, wipe_blobs_in};
use crate::store_gc::{GcError, blob_stem, collect_referenced_hashes, read_dir};

/// Telemetry sink filename under `.basemind/`. Mirrors
/// [`crate::mcp::telemetry::TELEMETRY_FILENAME`]; duplicated here to avoid a dependency on
/// the MCP module from the cleanup layer.
pub(crate) const TELEMETRY_FILENAME: &str = "telemetry.jsonl";

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
    /// Blob files whose hex stem is referenced by no view — reclaimable by
    /// [`run_gc`](crate::store_gc::run_gc). Meaningful only when [`Self::blob_accounting_ok`] is
    /// `true`; `0` otherwise (not computed).
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

/// Clear a whole cache component. Reuses the store's existing wipe helpers where they
/// exist; mirrors the lance dir-wipe pattern for the (feature-gated) vector store.
///
/// `Blobs` clears the machine-global blob store (shared by every workspace); all other components
/// are per-workspace under `basemind_dir`.
pub fn clear_component(basemind_dir: &Path, component: CacheComponent) -> Result<(), GcError> {
    clear_component_in(basemind_dir, component, &global_blobs_dir())
}

/// [`clear_component`] against an explicit blob directory (used for the `Blobs` branch). Production
/// passes the global store; unit tests pass a per-test temp dir so a `Blobs` clear never wipes the
/// machine-global store nor races sibling tests that rely on content-addressed blobs surviving.
pub(crate) fn clear_component_in(
    basemind_dir: &Path,
    component: CacheComponent,
    blobs_dir: &Path,
) -> Result<(), GcError> {
    match component {
        CacheComponent::Blobs => wipe_blobs_in(blobs_dir)?,
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
/// shared with other views. Run [`run_gc`](crate::store_gc::run_gc) afterwards to reclaim any
/// now-orphaned blobs.
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
    cache_stats_in(basemind_dir, &global_blobs_dir())
}

/// [`cache_stats`] against an explicit blob directory. Production passes the global store; unit
/// tests pass a per-test temp dir so they neither read the machine-global blob store nor race
/// each other on it.
pub(crate) fn cache_stats_in(basemind_dir: &Path, blobs_dir: &Path) -> Result<CacheStats, GcError> {
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
        for entry in read_dir(blobs_dir)? {
            let entry = entry.map_err(|source| GcError::Io {
                path: blobs_dir.to_path_buf(),
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
            if let Some(referenced) = &referenced
                && !referenced.contains(stem)
            {
                orphan_blob_count += 1;
            }
        }
    }

    let blobs_bytes = dir_size(blobs_dir)?;
    let views_bytes = dir_size(&basemind_dir.join(VIEWS_DIR))?;
    let lance_bytes = dir_size(&basemind_dir.join("lance"))?;
    let git_cache_bytes = dir_size(&basemind_dir.join(crate::git_cache::GIT_CACHE_DIR))?;
    let telemetry_bytes = file_size(&basemind_dir.join(TELEMETRY_FILENAME))?;
    let git_history_bytes = dir_size(&basemind_dir.join(crate::git_history::GIT_HISTORY_DIR))?;

    let total_bytes = dir_size(basemind_dir)? + blobs_bytes;
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
pub(crate) fn dir_size(dir: &Path) -> Result<u64, GcError> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut total = std::fs::symlink_metadata(dir).map(|m| on_disk_size(&m)).unwrap_or(0);
    for entry in read_dir(dir)? {
        let entry = entry.map_err(|source| GcError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(meta) => meta,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(GcError::Io {
                    path: path.clone(),
                    source,
                });
            }
        };
        if meta.is_dir() {
            total += dir_size(&path)?;
        } else {
            total += on_disk_size(&meta);
        }
    }
    Ok(total)
}
