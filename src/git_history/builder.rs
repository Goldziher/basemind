//! Build, incrementally append to, and revalidate the git-history index.
//!
//! Parity guarantee: the index is built from [`Repo::commit_record`], which returns the same
//! union-across-parents, exact-path, no-rename-follow relation the live history tools filter on —
//! so an indexed query result is identical to the live walk it replaces.

use std::path::Path;

use ahash::AHashMap;
use rayon::prelude::*;

use super::{CommitMeta, GitHistoryError, GitHistoryIndex, encoding, keys};
use crate::git::Repo;
use crate::path::RelPath;

/// What `sync` did, so callers (and tests) can tell a rewrite-triggered rebuild from a cheap
/// incremental append or a no-op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebuildOutcome {
    /// HEAD unchanged since the last index — nothing to do.
    Fresh,
    /// Appended `added` new commits reachable from the new HEAD.
    Incremental { added: u32 },
    /// Wiped and rebuilt from scratch (`reason` describes why), indexing `commits` commits.
    FullRebuild { reason: &'static str, commits: u32 },
}

/// Bring the index up to date with `repo`'s current HEAD, running the revalidation decision tree.
/// Best-effort callers ignore the error; the index simply stays stale and tools fall back to the
/// live walk until the next successful sync.
pub fn sync(
    index: &GitHistoryIndex,
    repo: &Repo,
    basemind_dir: &Path,
) -> Result<RebuildOutcome, GitHistoryError> {
    let head = match repo.resolve_rev("HEAD") {
        Ok(h) => h,
        Err(_) => return Ok(RebuildOutcome::Fresh), // unborn HEAD: nothing to index
    };

    // 1. Fresh — HEAD matches the last indexed head.
    if index.last_indexed_head_hex().as_deref() == Some(head.as_str()) {
        return Ok(RebuildOutcome::Fresh);
    }

    // 2. Incremental — last head still resolves, is an ancestor of HEAD, and the fingerprint holds.
    if let Some(last_hex) = index.last_indexed_head_hex()
        && repo.has_commit(&last_hex)
        && repo.is_ancestor(&last_hex, &head)
        && fingerprint_ok(index, repo, &head)
    {
        return append_since(index, repo, &last_hex, &head);
    }

    // 3. Full rebuild — never indexed, or history was rewritten / diverged (filter-repo, rebase,
    //    force-push, reset-back). Wipe first so stale shas can't survive.
    let reason = if index.is_empty() {
        "initial"
    } else {
        "history-rewrite"
    };
    index.clear(basemind_dir)?;
    rebuild(index, repo, reason)
}

/// Defense-in-depth: even when the ancestry check passes, a rewrite below `last_head` (graft,
/// replace-objects, unshallow) is caught when the stored root sha no longer resolves or is no
/// longer an ancestor of HEAD.
fn fingerprint_ok(index: &GitHistoryIndex, repo: &Repo, head: &str) -> bool {
    match index.root_sha() {
        None => false,
        Some(root_raw) => {
            let root_hex = keys::sha_raw_to_hex(&root_raw);
            repo.has_commit(&root_hex) && repo.is_ancestor(&root_hex, head)
        }
    }
}

/// Full rebuild over all commits reachable from HEAD.
fn rebuild(
    index: &GitHistoryIndex,
    repo: &Repo,
    reason: &'static str,
) -> Result<RebuildOutcome, GitHistoryError> {
    let newest_first = repo.all_commit_shas()?;
    if newest_first.is_empty() {
        return Ok(RebuildOutcome::FullRebuild { reason, commits: 0 });
    }
    let head20 = keys::sha_hex_to_raw(&newest_first[0]);
    let root20 = keys::sha_hex_to_raw(&newest_first[newest_first.len() - 1]);
    let (Some(head20), Some(root20)) = (head20, root20) else {
        return Ok(RebuildOutcome::FullRebuild { reason, commits: 0 });
    };

    // Chronological (oldest = ord 0) so ordinals are append-friendly for later incremental syncs.
    let chrono: Vec<&String> = newest_first.iter().rev().collect();
    let total = chrono.len() as u32;
    let mut interner = PathInterner::new(index, 0);
    let mut postings: AHashMap<u32, Vec<u32>> = AHashMap::new();
    let mut writer = index.writer();

    let written = fold_chunked(
        index,
        repo,
        &chrono,
        0,
        false,
        &mut interner,
        &mut postings,
        &mut writer,
    )?;
    for (path_id, ords) in postings {
        writer.put_posting(path_id, &encoding::encode_ords(&ords))?;
    }
    writer.finish_meta(&head20, &root20, total, interner.next_path_id, written)?;
    Ok(RebuildOutcome::FullRebuild {
        reason,
        commits: written,
    })
}

/// Append the commits reachable from HEAD but not from `last_hex`.
fn append_since(
    index: &GitHistoryIndex,
    repo: &Repo,
    last_hex: &str,
    head: &str,
) -> Result<RebuildOutcome, GitHistoryError> {
    let new_newest_first = repo.new_commit_shas(last_hex)?;
    let Some(head20) = keys::sha_hex_to_raw(head) else {
        return Ok(RebuildOutcome::Fresh);
    };
    let root20 = index.root_sha().unwrap_or(head20);
    let start_ord = index.next_ord();

    if new_newest_first.is_empty() {
        // HEAD moved but no new commits are reachable — just advance the head pointer.
        let writer = index.writer();
        writer.finish_meta(
            &head20,
            &root20,
            start_ord,
            index.next_path_id(),
            index.commit_count(),
        )?;
        return Ok(RebuildOutcome::Incremental { added: 0 });
    }

    let chrono: Vec<&String> = new_newest_first.iter().rev().collect();
    let mut interner = PathInterner::new(index, index.next_path_id());
    let mut postings: AHashMap<u32, Vec<u32>> = AHashMap::new();
    let mut writer = index.writer();

    let added = fold_chunked(
        index,
        repo,
        &chrono,
        start_ord,
        true,
        &mut interner,
        &mut postings,
        &mut writer,
    )?;
    // New ordinals are all greater than any existing one, so appending keeps each posting list
    // ascending — read the existing list, extend, re-encode.
    for (path_id, new_ords) in postings {
        let mut all = index
            .posting_bytes(path_id)
            .map(|b| encoding::decode_ords(&b))
            .unwrap_or_default();
        all.extend(new_ords);
        writer.put_posting(path_id, &encoding::encode_ords(&all))?;
    }
    let next_ord = start_ord + chrono.len() as u32;
    writer.finish_meta(
        &head20,
        &root20,
        next_ord,
        interner.next_path_id,
        index.commit_count() + added,
    )?;
    Ok(RebuildOutcome::Incremental { added })
}

/// Records computed (in parallel) per chunk before being folded and dropped. Bounds peak RSS: the
/// records hold a full `RelPath` per change edge, so materializing the whole history at once was the
/// build's dominant allocation (multi-GB on a 200k-commit monorepo). At `RECORD_CHUNK` commits per
/// batch only that slice is resident; the interner and posting accumulator (which must persist
/// across the whole run) are a small fraction of the former peak. Large enough to keep rayon's
/// worker pool saturated on the expensive `commit_record` diff.
const RECORD_CHUNK: usize = 8192;

/// Fold `chrono` (oldest-first) into the writer and posting accumulator in memory-bounded chunks.
/// Each chunk's records are computed in parallel via [`compute_records`], folded serially (interning
/// then Fjall puts), then dropped before the next chunk is computed. Ordinals are assigned by their
/// absolute position in `chrono` (`start_ord` plus the global offset) so a `None`/unparseable record
/// still consumes its slot, preserving the positional ordinal contract the non-chunked fold had.
/// Returns the number of commits actually written. `dedup` skips commits already present in
/// `gh_ord_by_sha` (the incremental append's defensive guard).
#[allow(clippy::too_many_arguments)] // builder-internal fold; threading shared accumulators by ref
fn fold_chunked(
    index: &GitHistoryIndex,
    repo: &Repo,
    chrono: &[&String],
    start_ord: u32,
    dedup: bool,
    interner: &mut PathInterner,
    postings: &mut AHashMap<u32, Vec<u32>>,
    writer: &mut super::GitHistoryWriter,
) -> Result<u32, GitHistoryError> {
    let mut written = 0u32;
    for (chunk_index, chunk) in chrono.chunks(RECORD_CHUNK).enumerate() {
        let records = compute_records(repo, chunk);
        let base = chunk_index * RECORD_CHUNK;
        for (offset, record) in records.into_iter().enumerate() {
            let ord = start_ord + (base + offset) as u32;
            let Some(record) = record else { continue };
            let Some(sha20) = keys::sha_hex_to_raw(&record.sha) else {
                continue;
            };
            if dedup && index.ord_for_sha(&sha20).is_some() {
                continue; // already indexed (defensive dedup)
            }
            let files = intern_files(interner, postings, ord, &record.files, writer)?;
            let meta = CommitMeta {
                sha: record.sha,
                summary: record.summary,
                author: record.author,
                author_time_unix: record.author_time_unix,
                files,
            };
            writer.put_commit_meta(ord, &meta)?;
            writer.put_ord_for_sha(&sha20, ord)?;
            written += 1;
        }
    }
    Ok(written)
}

/// Compute a chunk of commits' full records in parallel. Each rayon worker gets its own thread-local
/// gix repository via `Repo::commit_record` → `Repo::local`, so the `!Sync` gix repo is never shared.
fn compute_records(repo: &Repo, chrono: &[&String]) -> Vec<Option<crate::git::CommitInfo>> {
    chrono
        .par_iter()
        .map(|sha| repo.commit_record(sha))
        .collect()
}

/// Intern a commit's file paths to `path_id`s, recording the new path rows and posting edges.
fn intern_files(
    interner: &mut PathInterner,
    postings: &mut AHashMap<u32, Vec<u32>>,
    ord: u32,
    files: &[(RelPath, crate::git::ChangeKind)],
    writer: &mut super::GitHistoryWriter,
) -> Result<Vec<(u32, u8)>, GitHistoryError> {
    let mut out = Vec::with_capacity(files.len());
    for (rel, kind) in files {
        let path_id = interner.intern(rel, writer)?;
        out.push((path_id, keys::change_kind_byte(*kind)));
        postings.entry(path_id).or_default().push(ord);
    }
    Ok(out)
}

/// Assigns dense `path_id`s, reusing ids already persisted in the index (for incremental syncs) and
/// caching lookups in RAM. Writes a `gh_path_id_by_path` + `gh_path_by_id` row the first time a path
/// is seen in this run.
struct PathInterner<'a> {
    index: &'a GitHistoryIndex,
    cache: AHashMap<RelPath, u32>,
    next_path_id: u32,
}

impl<'a> PathInterner<'a> {
    fn new(index: &'a GitHistoryIndex, next_path_id: u32) -> Self {
        Self {
            index,
            cache: AHashMap::new(),
            next_path_id,
        }
    }

    fn intern(
        &mut self,
        rel: &RelPath,
        writer: &mut super::GitHistoryWriter,
    ) -> Result<u32, GitHistoryError> {
        if let Some(&id) = self.cache.get(rel) {
            return Ok(id);
        }
        if let Some(id) = self.index.path_id(rel) {
            self.cache.insert(rel.clone(), id);
            return Ok(id);
        }
        let id = self.next_path_id;
        self.next_path_id += 1;
        self.cache.insert(rel.clone(), id);
        writer.put_path(rel, id)?;
        Ok(id)
    }
}
