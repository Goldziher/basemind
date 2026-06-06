use std::path::{Path, PathBuf};
use std::time::SystemTime;

use globset::{Glob, GlobSetBuilder};
use ignore::WalkBuilder;
use rayon::prelude::*;
use thiserror::Error;
use tracing::debug;

use crate::config::Config;
use crate::extract::{ExtractError, FileMapL1, l1};
use crate::git::{GitError, Repo};
use crate::hashing;
use crate::lang::{self, Lang};
use crate::store::{FileEntry, Store, StoreError};

/// What state of the repository the scanner indexes from.
///
/// - `WorkingTree` (today's default) — walk the filesystem via `ignore::WalkBuilder`,
///   read bytes via `std::fs::read`.
/// - `Staged` — list paths from the git index, read blob bytes from the index. Lets the
///   pre-commit hook index *what is about to be committed* rather than whatever stale work
///   is sitting in the working tree.
/// - `Rev { sha }` — list the tree at `sha`, read blob bytes from that tree.
#[derive(Clone)]
pub enum ScanSource<'a> {
    WorkingTree,
    Staged(&'a Repo),
    Rev { repo: &'a Repo, sha: String },
}

impl<'a> ScanSource<'a> {
    fn label(&self) -> String {
        match self {
            ScanSource::WorkingTree => "working tree".to_string(),
            ScanSource::Staged(_) => "staged index".to_string(),
            ScanSource::Rev { sha, .. } => format!("rev {}", &sha[..7.min(sha.len())]),
        }
    }
}

#[derive(Debug, Error)]
pub enum ScanError {
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("invalid glob in config: {0}")]
    BadGlob(String),
    #[error("git error: {0}")]
    Git(#[from] GitError),
}

/// Aggregate counters for a single scan invocation.
/// Computed from the per-file results; kept for backwards-compat assertions in tests.
#[derive(Debug, Default, Clone, Copy)]
pub struct ScanStats {
    pub scanned: usize,
    pub updated: usize,
    pub updated_with_warnings: usize,
    pub skipped_unchanged: usize,
    pub skipped_too_large: usize,
    pub skipped_non_utf8: usize,
    pub skipped_no_lang: usize,
    pub removed: usize,
    pub read_failed: usize,
    pub extract_failed: usize,
}

/// Per-file result. Every file the scanner *considered* shows up here.
/// SkippedNoLang is included so callers can render or hide it via verbosity.
#[derive(Debug, Clone)]
pub struct FileResult {
    /// Relative path, forward-slash separated.
    pub path: String,
    pub status: FileStatus,
    /// Internal: buffered FileEntry when the file was updated. The parallel `process_file`
    /// stashes the entry here; the single-threaded apply loop drains it into the store.
    /// Not part of the public surface — always `None` once `apply_outcomes` returns.
    pub(crate) upsert: Option<FileEntry>,
}

#[derive(Debug, Clone)]
pub enum FileStatus {
    Updated {
        had_errors: bool,
        error_count: u32,
    },
    Unchanged,
    Removed,
    SkippedTooLarge {
        size: u64,
    },
    SkippedNonUtf8,
    SkippedNoLang,
    ReadFailed {
        kind: std::io::ErrorKind,
        msg: String,
    },
    ExtractFailed {
        msg: String,
    },
}

#[derive(Debug, Clone, Default)]
pub struct ScanReport {
    pub results: Vec<FileResult>,
    pub stats: ScanStats,
}

struct Filters {
    include: globset::GlobSet,
    exclude: globset::GlobSet,
    max_file_bytes: u64,
}

impl Filters {
    fn build(config: &Config) -> Result<Self, ScanError> {
        let include = compile_globs(&config.scan.include)?;
        let exclude = compile_globs(&config.scan.exclude)?;
        Ok(Self {
            include,
            exclude,
            max_file_bytes: config.scan.max_file_bytes,
        })
    }

    fn allows(&self, rel: &str) -> bool {
        if self.exclude.is_match(rel) {
            return false;
        }
        self.include.is_match(rel)
    }
}

fn compile_globs(patterns: &[String]) -> Result<globset::GlobSet, ScanError> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        let g = Glob::new(p).map_err(|e| ScanError::BadGlob(format!("{p:?}: {e}")))?;
        b.add(g);
    }
    b.build().map_err(|e| ScanError::BadGlob(format!("{e}")))
}

/// One-shot scan: enumerate every candidate file *via the requested source*, process them
/// in parallel, purge stale index entries, flush the index, return a typed report.
///
/// Source-aware behavior:
/// - `WorkingTree` uses `ignore::WalkBuilder` to walk the on-disk tree and `std::fs::read`.
/// - `Staged` and `Rev` enumerate paths via gix and read bytes via gix.
pub fn scan(
    root: &Path,
    store: &mut Store,
    config: &Config,
    source: ScanSource<'_>,
) -> Result<ScanReport, ScanError> {
    let filters = Filters::build(config)?;
    let candidates = candidates_for_source(root, config, &filters, &source)?;
    debug!(
        count = candidates.len(),
        kind = source.label(),
        "scan candidates"
    );

    let outcomes: Vec<FileResult> = candidates
        .par_iter()
        .map(|rel| process_file(root, rel, &filters, store, &source))
        .collect();

    let seen: ahash::AHashSet<String> = outcomes
        .iter()
        .filter_map(|r| match &r.status {
            FileStatus::Updated { .. } | FileStatus::Unchanged => Some(r.path.clone()),
            _ => None,
        })
        .collect();

    let mut report = ScanReport::default();
    apply_outcomes(store, &mut report, outcomes);

    // Purge index entries for files no longer present / no longer allowed.
    let stale: Vec<String> = store
        .index
        .files
        .keys()
        .filter(|k| !seen.contains(k.as_str()))
        .cloned()
        .collect();
    for k in &stale {
        store.remove(k);
        report.results.push(FileResult {
            path: k.clone(),
            status: FileStatus::Removed,
            upsert: None,
        });
        report.stats.removed += 1;
    }

    store.flush()?;
    Ok(report)
}

/// Incremental scan: process only the given absolute paths. Used by the watcher
/// where the debouncer already told us which files changed.
///
/// Paths outside `root`, inside `.gitmind/`, or not matching the include globs are
/// silently dropped (the watcher pre-filters but we re-check defensively).
/// Removed files (path no longer exists) are purged from the index.
pub fn scan_paths(
    root: &Path,
    store: &mut Store,
    config: &Config,
    paths: &[PathBuf],
) -> Result<ScanReport, ScanError> {
    let filters = Filters::build(config)?;
    let source = ScanSource::WorkingTree;

    let mut rels: Vec<String> = Vec::with_capacity(paths.len());
    let mut removed: Vec<String> = Vec::new();
    for abs in paths {
        let rel = match abs.strip_prefix(root) {
            Ok(p) => p.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        if rel.is_empty() || rel.starts_with(crate::config::GITMIND_DIR) {
            continue;
        }
        if !abs.exists() {
            if store.lookup(&rel).is_some() {
                removed.push(rel);
            }
            continue;
        }
        if !filters.allows(&rel) {
            continue;
        }
        rels.push(rel);
    }
    rels.sort();
    rels.dedup();

    let outcomes: Vec<FileResult> = rels
        .par_iter()
        .map(|rel| process_file(root, rel, &filters, store, &source))
        .collect();

    let mut report = ScanReport::default();
    apply_outcomes(store, &mut report, outcomes);

    for rel in removed {
        store.remove(&rel);
        report.results.push(FileResult {
            path: rel,
            status: FileStatus::Removed,
            upsert: None,
        });
        report.stats.removed += 1;
    }

    store.flush()?;
    Ok(report)
}

fn apply_outcomes(store: &mut Store, report: &mut ScanReport, outcomes: Vec<FileResult>) {
    for o in outcomes {
        report.stats.scanned += 1;
        match &o.status {
            FileStatus::Updated {
                had_errors,
                error_count: _,
            } => {
                report.stats.updated += 1;
                if *had_errors {
                    report.stats.updated_with_warnings += 1;
                }
                // The entry update was already buffered by process_file via the side
                // channel below. We can't safely mutate the store from inside the
                // parallel map, so process_file stashes the entry on the FileResult.
            }
            FileStatus::Unchanged => report.stats.skipped_unchanged += 1,
            FileStatus::SkippedTooLarge { .. } => report.stats.skipped_too_large += 1,
            FileStatus::SkippedNonUtf8 => report.stats.skipped_non_utf8 += 1,
            FileStatus::SkippedNoLang => report.stats.skipped_no_lang += 1,
            FileStatus::Removed => report.stats.removed += 1,
            FileStatus::ReadFailed { .. } => report.stats.read_failed += 1,
            FileStatus::ExtractFailed { .. } => report.stats.extract_failed += 1,
        }
        // Pull buffered entry off the result, if any, and upsert it into the index.
        if let Some(entry) = o.upsert.clone() {
            store.upsert(&o.path, entry);
        }
        let cleared = FileResult {
            path: o.path,
            status: o.status,
            upsert: None,
        };
        report.results.push(cleared);
    }
}

fn candidates_for_source(
    root: &Path,
    config: &Config,
    filters: &Filters,
    source: &ScanSource<'_>,
) -> Result<Vec<String>, ScanError> {
    let raw = match source {
        ScanSource::WorkingTree => walk_candidates(root, config, filters),
        ScanSource::Staged(repo) => repo.list_paths_staged()?,
        ScanSource::Rev { repo, sha } => repo.list_paths_rev(sha)?,
    };
    // For git sources we still apply the configured include/exclude filters so the user can
    // turn things off via `.gitmind/gitmind.toml`.
    let mut out: Vec<String> = match source {
        ScanSource::WorkingTree => raw,
        _ => raw
            .into_iter()
            .filter(|rel| filters.allows(rel))
            .filter(|rel| !rel.starts_with(crate::config::GITMIND_DIR))
            .collect(),
    };
    out.sort();
    out.dedup();
    Ok(out)
}

fn walk_candidates(root: &Path, config: &Config, filters: &Filters) -> Vec<String> {
    let mut out = Vec::new();
    let walker = WalkBuilder::new(root)
        .standard_filters(config.scan.respect_gitignore)
        .follow_links(false)
        .git_ignore(config.scan.respect_gitignore)
        .git_exclude(config.scan.respect_gitignore)
        .hidden(false)
        .build();
    for dent in walker.flatten() {
        if !dent.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = dent.path();
        let rel = match path.strip_prefix(root) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if !filters.allows(&rel_str) {
            continue;
        }
        out.push(rel_str);
    }
    out
}

/// Process a single relative path. Returns a `FileResult`; if the file is being
/// updated, the new `FileEntry` is attached via `FileResult::upsert` so the caller
/// can apply it to the store from the single-threaded apply loop.
fn process_file(
    root: &Path,
    rel: &str,
    filters: &Filters,
    store: &Store,
    source: &ScanSource<'_>,
) -> FileResult {
    let lang = match lang::detect(Path::new(rel)) {
        Some(l) => l,
        None => {
            return FileResult {
                path: rel.to_string(),
                status: FileStatus::SkippedNoLang,
                upsert: None,
            };
        }
    };

    // Source-aware byte read + size check + mtime.
    let (bytes, size_bytes, mtime) = match source {
        ScanSource::WorkingTree => match read_working_tree(root, rel, filters) {
            Ok(triple) => triple,
            Err(status) => {
                return FileResult {
                    path: rel.to_string(),
                    status,
                    upsert: None,
                };
            }
        },
        ScanSource::Staged(repo) => match read_via_git(filters, repo.read_blob_staged(rel)) {
            Ok(triple) => triple,
            Err(status) => {
                return FileResult {
                    path: rel.to_string(),
                    status,
                    upsert: None,
                };
            }
        },
        ScanSource::Rev { repo, sha } => {
            match read_via_git(filters, repo.read_blob_at_rev(sha, rel)) {
                Ok(triple) => triple,
                Err(status) => {
                    return FileResult {
                        path: rel.to_string(),
                        status,
                        upsert: None,
                    };
                }
            }
        }
    };

    if std::str::from_utf8(&bytes).is_err() {
        return FileResult {
            path: rel.to_string(),
            status: FileStatus::SkippedNonUtf8,
            upsert: None,
        };
    }

    let hash = hashing::hash_bytes(&bytes);
    let hash_hex = hashing::hex(&hash);

    if let Some(existing) = store.lookup(rel)
        && existing.hash_hex == hash_hex
        && store.blob_path_l1(&hash).exists()
    {
        return FileResult {
            path: rel.to_string(),
            status: FileStatus::Unchanged,
            upsert: None,
        };
    }

    let l1: FileMapL1 = match l1::extract_l1(lang, &bytes) {
        Ok(m) => m,
        Err(source) => {
            return FileResult {
                path: rel.to_string(),
                status: FileStatus::ExtractFailed {
                    msg: format_extract_err(&source),
                },
                upsert: None,
            };
        }
    };

    if let Err(e) = store.write_l1(&hash, &l1) {
        return FileResult {
            path: rel.to_string(),
            status: FileStatus::ExtractFailed { msg: e.to_string() },
            upsert: None,
        };
    }

    let entry = FileEntry {
        hash_hex,
        language: lang_name(lang).to_string(),
        size_bytes,
        mtime,
    };
    FileResult {
        path: rel.to_string(),
        status: FileStatus::Updated {
            had_errors: l1.had_errors,
            error_count: l1.error_count,
        },
        upsert: Some(entry),
    }
}

fn read_working_tree(
    root: &Path,
    rel: &str,
    filters: &Filters,
) -> Result<(Vec<u8>, u64, i64), FileStatus> {
    let abs = root.join(rel);
    let metadata = std::fs::metadata(&abs).map_err(|e| FileStatus::ReadFailed {
        kind: e.kind(),
        msg: e.to_string(),
    })?;
    if metadata.len() > filters.max_file_bytes {
        return Err(FileStatus::SkippedTooLarge {
            size: metadata.len(),
        });
    }
    let bytes = std::fs::read(&abs).map_err(|e| FileStatus::ReadFailed {
        kind: e.kind(),
        msg: e.to_string(),
    })?;
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let size = metadata.len();
    Ok((bytes, size, mtime))
}

fn read_via_git(
    filters: &Filters,
    blob: Result<Option<Vec<u8>>, GitError>,
) -> Result<(Vec<u8>, u64, i64), FileStatus> {
    let blob = blob.map_err(|e| FileStatus::ReadFailed {
        kind: std::io::ErrorKind::Other,
        msg: e.to_string(),
    })?;
    let bytes = blob.ok_or(FileStatus::ReadFailed {
        kind: std::io::ErrorKind::NotFound,
        msg: "blob not present in this git source".to_string(),
    })?;
    if bytes.len() as u64 > filters.max_file_bytes {
        return Err(FileStatus::SkippedTooLarge {
            size: bytes.len() as u64,
        });
    }
    let size = bytes.len() as u64;
    // Git sources don't have an mtime. 0 just means "unknown" — the existing hash-equality
    // check is what actually decides whether to re-extract.
    Ok((bytes, size, 0))
}

fn format_extract_err(e: &ExtractError) -> String {
    e.to_string()
}

fn lang_name(l: Lang) -> &'static str {
    l.name()
}
