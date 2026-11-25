//! Build, incrementally append to, and revalidate the git-history index.
//!
//! The commit store is head-independent. A sync stores the commits reachable from the caller's HEAD
//! that no earlier sync stored, and records what HEAD reaches as a tip; it never wipes because HEAD
//! diverged from a head another worktree indexed.
//!
//! Parity guarantee: the index is built from [`Repo::commit_record`], which returns the same
//! exact-path, no-rename-follow relation the live history tools filter on (a merge lists only the
//! files it changed against every parent, as `git log --cc` does), so an indexed query result is
//! identical to the live walk it replaces.

use std::path::Path;

use ahash::AHashMap;
use rayon::prelude::*;

use super::reach::Reach;
use super::{CommitMeta, GitHistoryError, GitHistoryIndex, encoding, keys};
use crate::git::Repo;
use crate::path::RelPath;

/// What `sync` did, so callers (and tests) can tell a rebuild from a cheap incremental append or a
/// no-op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebuildOutcome {
    /// HEAD is already an indexed tip — no commit is walked.
    Fresh,
    /// HEAD became an indexed tip by appending the `added` commits reachable from it that were not
    /// stored yet (none when every commit was already stored for another head).
    Incremental { added: u32 },
    /// The index was left untouched: a build at this head failed within the hour (`reason` says so),
    /// so tools read via the live walk until it is retried.
    Skipped { reason: &'static str },
    /// Wiped and rebuilt from scratch for this HEAD, indexing `commits` commits. `reason` is
    /// `"initial"` (nothing was indexed), `"interrupted"` (a build did not finish) or `"deepened"`
    /// (the shallow clone it was built in has been deepened or unshallowed).
    FullRebuild { reason: &'static str, commits: u32 },
}

/// Bring the index up to date with `repo`'s current HEAD: make HEAD an indexed tip, appending the
/// commits not stored yet.
/// Best-effort callers ignore the error; the index simply stays stale and tools fall back to the
/// live walk until the next successful sync.
/// Only the process that HOLDS the fjall database may build it. Syncs of one database serialize on
/// its build lock, which every clone of the handle shares: callers that queue behind a sync find the
/// index fresh and return at once.
///
/// And only a repository the caller actually rooted at is built: `repo` reaching here from a
/// workspace root *below* its workdir means an ancestor repository was inherited, which
/// [`history_scope_ok`](super::history_scope_ok) refuses.
pub fn sync(index: &GitHistoryIndex, repo: &Repo, basemind_dir: &Path) -> Result<RebuildOutcome, GitHistoryError> {
    sync_with(index, repo, basemind_dir, &Unattended)
}

/// [`sync`] steered by `control`: asked between chunks of commits whether to stop, and told how far
/// the walk got. A sync that succeeds owes the background nothing, so it removes the progress
/// record of a handed-off build in `basemind_dir` ([`super::progress`]).
pub fn sync_with(
    index: &GitHistoryIndex,
    repo: &Repo,
    basemind_dir: &Path,
    control: &dyn BuildControl,
) -> Result<RebuildOutcome, GitHistoryError> {
    let _building = index
        .local()
        .build_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Run on a small dedicated pool, not rayon's global one: a first build walks hundreds of thousands
    // of commits in parallel, and on the global pool it starves every other par_iter in the process
    // (a read-stack warm that takes 12 s took 469 s while a 248k-commit build was running).
    // Under thread exhaustion the pool cannot be built; degrade to the caller's thread (slower, and on
    // rayon's global pool) rather than panicking.
    let outcome = match history_pool() {
        Some(pool) => pool.install(|| sync_inner(index, repo, control)),
        None => sync_inner(index, repo, control),
    };
    if outcome.is_ok() {
        super::progress::clear(basemind_dir);
    }
    outcome
}

/// Threads given to history indexing. Enough to finish a first build in minutes without taking the
/// scan and warm paths' cores.
const HISTORY_POOL_THREADS: usize = 2;

fn history_pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: std::sync::OnceLock<Option<rayon::ThreadPool>> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(HISTORY_POOL_THREADS)
            .thread_name(|i| format!("bm-history-{i}"))
            .build()
            .map_err(|error| tracing::warn!(%error, "git-history pool unavailable; syncing on the caller's thread"))
            .ok()
    })
    .as_ref()
}

/// How a caller steers a build or append from outside it.
pub trait BuildControl: Sync {
    /// Whether the walk stops before its next chunk of commits. A stopped walk fails with
    /// [`GitHistoryError::Cancelled`] and leaves the index marked in progress: no reader trusts it,
    /// and the next sync rebuilds it.
    fn stop(&self) -> bool {
        false
    }

    /// `done` of the `total` commits the walk stores are stored.
    fn progress(&self, _done: u32, _total: u32) {}
}

/// The control of a sync nobody steers: it never stops and reports nothing.
struct Unattended;

impl BuildControl for Unattended {}

/// What the next sync of an index does, decided before it walks a commit.
enum Plan {
    /// HEAD does not resolve: nothing to index.
    NoHead,
    /// HEAD is an indexed tip.
    Current(String),
    /// HEAD becomes a tip by appending the commits not stored yet.
    Append(String),
    /// A build at HEAD failed within the hour.
    BackingOff,
    /// Wiped and rebuilt from scratch, for `reason`.
    Rebuild { reason: &'static str, head: String },
}

fn plan(index: &GitHistoryIndex, repo: &Repo) -> Result<Plan, GitHistoryError> {
    if !super::history_scope_ok(repo) {
        return Err(GitHistoryError::ForeignHistory {
            root: repo.origin().to_path_buf(),
            workdir: repo.workdir().to_path_buf(),
        });
    }
    let Ok(head) = repo.resolve_rev("HEAD") else {
        return Ok(Plan::NoHead);
    };
    let deepened = root_deepened(index, repo);
    if !deepened && index.has_tip(&head) {
        return Ok(Plan::Current(head));
    }
    // Any other state with a stamped head and nothing deepened is safe to extend, whatever the
    // relation of HEAD to the tips already indexed: the store is head-independent, so a divergence
    // appends the commits it adds and wipes nothing. An interrupted index has no stamped head and so
    // reads as empty here, as does one never built.
    if !deepened && !index.is_empty() {
        return Ok(Plan::Append(head));
    }
    let interrupted = index.build_interrupted();
    if interrupted && backing_off(index, &head, now_unix()) {
        return Ok(Plan::BackingOff);
    }
    let reason = if interrupted {
        "interrupted"
    } else if index.is_empty() {
        "initial"
    } else {
        "deepened"
    };
    Ok(Plan::Rebuild { reason, head })
}

/// Why the next sync of `index` rebuilds it from scratch (`"initial"`, `"interrupted"` or
/// `"deepened"`, as [`RebuildOutcome::FullRebuild`] names it), or `None` when it finds HEAD
/// indexed, appends to it, backs off a failed build or refuses the repository. Walks no commit: a
/// caller asks this to hand the minutes-long rebuild of a deep history to the background.
pub fn full_rebuild_due(index: &GitHistoryIndex, repo: &Repo) -> Option<&'static str> {
    match plan(index, repo) {
        Ok(Plan::Rebuild { reason, .. }) => Some(reason),
        _ => None,
    }
}

fn sync_inner(
    index: &GitHistoryIndex,
    repo: &Repo,
    control: &dyn BuildControl,
) -> Result<RebuildOutcome, GitHistoryError> {
    match plan(index, repo)? {
        Plan::NoHead => Ok(RebuildOutcome::Fresh),
        Plan::Current(head) => {
            // Already indexed, perhaps for another worktree: make it the latest tip again, which a
            // read at no head answers for, and renew its sync time so eviction drops older tips first.
            if let Some(head20) = keys::sha_hex_to_raw(&head)
                && index.last_indexed_head() != Some(head20)
            {
                index.touch_tip(&head20)?;
            }
            Ok(RebuildOutcome::Fresh)
        }
        Plan::Append(head) => {
            append_tip(index, repo, &head, control).inspect_err(|error| record_failure(index, &head, error))
        }
        Plan::BackingOff => Ok(RebuildOutcome::Skipped {
            reason: "a build at this head failed within the hour",
        }),
        Plan::Rebuild { reason, head } => {
            index.clear()?;
            rebuild(index, repo, reason, control).inspect_err(|error| record_failure(index, &head, error))
        }
    }
}

/// How long a sync at the HEAD a build or append failed for leaves the index to the live walk. A
/// failure that recurs (a commit `commit_record` cannot read, a full disk) would otherwise cost a walk
/// of the whole history on every scan and every serve start.
const FAILED_BUILD_BACKOFF_SECS: u64 = 3600;

/// Whether the last failed build was for `head` and is younger than [`FAILED_BUILD_BACKOFF_SECS`].
fn backing_off(index: &GitHistoryIndex, head: &str, now: u64) -> bool {
    keys::sha_hex_to_raw(head)
        .and_then(|head20| index.failed_build_at(&head20))
        .is_some_and(|failed| now.saturating_sub(failed) < FAILED_BUILD_BACKOFF_SECS)
}

/// Record that the sync at `head` failed with `error`, unless it was stopped: a stopped walk failed
/// nothing, and is rebuilt by the next sync instead of being left to the live walk for the hour.
fn record_failure(index: &GitHistoryIndex, head: &str, error: &GitHistoryError) {
    if matches!(error, GitHistoryError::Cancelled) {
        return;
    }
    if let Some(head20) = keys::sha_hex_to_raw(head) {
        index.record_failed_build(&head20, now_unix());
    }
}

pub(super) fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Whether the oldest indexed commit now has a parent: the index was built in a shallow clone that
/// has since been deepened or unshallowed (`git fetch --unshallow` moves no ref), so it lacks the
/// history past its old boundary even while it indexes HEAD.
pub fn root_deepened(index: &GitHistoryIndex, repo: &Repo) -> bool {
    index
        .root_sha()
        .is_some_and(|root_raw| repo.has_readable_parent(&keys::sha_raw_to_hex(&root_raw)))
        || index.shallow_boundary() != repo.shallow_boundary()
}

/// Full rebuild over all commits reachable from HEAD.
fn rebuild(
    index: &GitHistoryIndex,
    repo: &Repo,
    reason: &'static str,
    control: &dyn BuildControl,
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

    let chrono: Vec<&String> = newest_first.iter().rev().collect();
    let total = chrono.len() as u32;
    let mut interner = PathInterner::new(index, 0);
    let mut postings: AHashMap<u32, Vec<u32>> = AHashMap::new();
    let mut term_postings: AHashMap<Vec<u8>, Vec<u32>> = AHashMap::new();
    let mut reach = Reach::default();
    let mut writer = index.writer()?;
    writer.begin()?;
    writer.put_shallow_boundary(&repo.shallow_boundary())?;

    let written = fold_chunked(
        repo,
        &chrono,
        0,
        &mut interner,
        &mut postings,
        &mut term_postings,
        &mut reach,
        &mut writer,
        control,
    )?;
    for (path_id, ords) in postings {
        writer.put_posting(path_id, &encoding::encode_ords(&ords))?;
    }
    for (term_key, ords) in term_postings {
        writer.put_term_posting(&term_key, &encoding::encode_ords(&ords))?;
    }
    // The ordinals follow the reverse of this very walk, one slot per commit.
    writer.set_ordered(true);
    writer.finish_meta(&head20, &reach, &root20, total, interner.next_path_id, written)?;
    index.local().compact()?;
    Ok(RebuildOutcome::FullRebuild {
        reason,
        commits: written,
    })
}

/// Make `head` an indexed tip, appending the commits reachable from it that are not stored yet.
///
/// What `head` reaches starts from the tip of an earlier sync that is its ancestor, when there is
/// one: only the commits it adds are walked. Otherwise (a branch off history indexed for another
/// head, or a HEAD moved back) the whole history reachable from it is walked, which costs one revision
/// walk and an ordinal lookup per commit but no diff for a commit that is already stored.
///
/// Every walk starts at `head` itself, never at the repository's HEAD again: HEAD may have moved
/// since the caller resolved it, and the commits walked must be the ones the tip is stamped for.
fn append_tip(
    index: &GitHistoryIndex,
    repo: &Repo,
    head: &str,
    control: &dyn BuildControl,
) -> Result<RebuildOutcome, GitHistoryError> {
    let Some(head20) = keys::sha_hex_to_raw(head) else {
        return Ok(RebuildOutcome::Fresh);
    };
    let (shas, mut reach, base) = match ancestor_tip(index, repo, head) {
        Some((base, reach)) => (
            repo.new_commit_shas_from(head, &keys::sha_raw_to_hex(&base))?,
            reach,
            Some(base),
        ),
        None => (repo.all_commit_shas_from(head)?, Reach::default(), None),
    };
    let mut unstored: Vec<&String> = Vec::new();
    // Each commit of the walk as its stored ordinal, or the position among the unstored ones.
    let mut walk: Vec<Result<u32, usize>> = Vec::with_capacity(shas.len());
    for sha in &shas {
        let Some(sha20) = keys::sha_hex_to_raw(sha) else {
            continue;
        };
        match index.ord_for_sha(&sha20) {
            Some(ord) => {
                reach.insert(ord);
                walk.push(Ok(ord));
            }
            None => {
                walk.push(Err(unstored.len()));
                unstored.push(sha);
            }
        }
    }
    let root20 = index.root_sha().unwrap_or(head20);
    let start_ord = index.next_ord();
    let ordered = match base {
        None => walk_descends(&walk, start_ord, unstored.len()) && walk.len() == shas.len(),
        Some(base) => {
            unstored.len() == shas.len()
                && index.tip_is_ordered(&base)
                && extends_linearly(repo, head, &shas, &keys::sha_raw_to_hex(&base))
        }
    };

    if unstored.is_empty() {
        let mut writer = index.writer()?;
        writer.set_ordered(ordered);
        writer.finish_meta(
            &head20,
            &reach,
            &root20,
            start_ord,
            index.next_path_id(),
            index.commit_count(),
        )?;
        return Ok(RebuildOutcome::Incremental { added: 0 });
    }

    let chrono: Vec<&String> = unstored.into_iter().rev().collect();
    let mut interner = PathInterner::new(index, index.next_path_id());
    let mut postings: AHashMap<u32, Vec<u32>> = AHashMap::new();
    let mut term_postings: AHashMap<Vec<u8>, Vec<u32>> = AHashMap::new();
    let mut writer = index.writer()?;
    writer.begin()?;

    let added = fold_chunked(
        repo,
        &chrono,
        start_ord,
        &mut interner,
        &mut postings,
        &mut term_postings,
        &mut reach,
        &mut writer,
        control,
    )?;
    for (path_id, new_ords) in postings {
        let mut all = index
            .posting_bytes(path_id)
            .map(|b| encoding::decode_ords(&b))
            .unwrap_or_default();
        all.extend(new_ords);
        writer.put_posting(path_id, &encoding::encode_ords(&all))?;
    }
    for (term_key, new_ords) in term_postings {
        let mut all = index
            .term_posting_bytes(&term_key)
            .map(|b| encoding::decode_ords(&b))
            .unwrap_or_default();
        all.extend(new_ords);
        writer.put_term_posting(&term_key, &encoding::encode_ords(&all))?;
    }
    let next_ord = start_ord + chrono.len() as u32;
    writer.set_ordered(ordered);
    writer.finish_meta(
        &head20,
        &reach,
        &root20,
        next_ord,
        interner.next_path_id,
        index.commit_count() + added,
    )?;
    Ok(RebuildOutcome::Incremental { added })
}

/// Whether a walk, newest first, maps to strictly descending ordinals, the unstored commits taking
/// the `count` ordinals from `start_ord` in reverse walk order (the oldest first).
fn walk_descends(walk: &[Result<u32, usize>], start_ord: u32, count: usize) -> bool {
    let ords = walk.iter().map(|slot| match slot {
        Ok(ord) => *ord,
        Err(position) => start_ord + (count - 1 - position) as u32,
    });
    let mut previous: Option<u32> = None;
    for ord in ords {
        if previous.is_some_and(|before| before <= ord) {
            return false;
        }
        previous = Some(ord);
    }
    true
}

/// Whether `shas` (the commits `head` adds over the tip `base`, newest first) are a linear chain:
/// `head`, then each one's only parent, ending at a commit whose only parent is `base`.
fn extends_linearly(repo: &Repo, head: &str, shas: &[String], base: &str) -> bool {
    let mut expected = head.to_owned();
    for sha in shas {
        if *sha != expected {
            return false;
        }
        match repo.parent_shas(sha).as_deref() {
            Some([parent]) => expected.clone_from(parent),
            _ => return false,
        }
    }
    !shas.is_empty() && expected == base
}

/// The most recently synced indexed tip that is an ancestor of `head`, with what it reaches.
fn ancestor_tip(index: &GitHistoryIndex, repo: &Repo, head: &str) -> Option<([u8; 20], Reach)> {
    index.tips().into_iter().find_map(|tip| {
        let tip20 = keys::sha_hex_to_raw(&tip)?;
        (repo.has_commit(&tip) && repo.is_ancestor(&tip, head))
            .then(|| index.reach_of(&tip20).map(|reach| (tip20, reach)))
            .flatten()
    })
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
/// absolute position in `chrono` (`start_ord` plus the global offset) so a `None`/unparsable record
/// still consumes its slot, preserving the positional ordinal contract the non-chunked fold had.
/// Returns the number of commits actually written; each one's ordinal is added to `reach`. Before
/// each chunk `control` is told how far the fold got and asked whether to stop. The
/// caller passes only commits no row exists for: a row a cut-off run left is never met, because such
/// a run leaves the index marked in progress and the next sync rebuilds it.
#[allow(clippy::too_many_arguments)]
fn fold_chunked(
    repo: &Repo,
    chrono: &[&String],
    start_ord: u32,
    interner: &mut PathInterner,
    postings: &mut AHashMap<u32, Vec<u32>>,
    term_postings: &mut AHashMap<Vec<u8>, Vec<u32>>,
    reach: &mut Reach,
    writer: &mut super::GitHistoryWriter,
    control: &dyn BuildControl,
) -> Result<u32, GitHistoryError> {
    let mut written = 0u32;
    let total = chrono.len() as u32;
    for (chunk_index, chunk) in chrono.chunks(RECORD_CHUNK).enumerate() {
        let base = chunk_index * RECORD_CHUNK;
        if control.stop() {
            return Err(GitHistoryError::Cancelled);
        }
        control.progress(base as u32, total);
        let records = compute_records(repo, chunk);
        for (offset, record) in records.into_iter().enumerate() {
            let ord = start_ord + (base + offset) as u32;
            let Some(record) = record else { continue };
            let Some(sha20) = keys::sha_hex_to_raw(&record.sha) else {
                continue;
            };
            let files = intern_files(interner, postings, ord, &record.files, writer)?;
            super::fts::index_commit_terms(
                term_postings,
                ord,
                &record.author,
                &record.author_email,
                &record.summary,
                &record.body,
            );
            if !record.body.is_empty() {
                writer.put_commit_text(ord, record.body.as_bytes())?;
            }
            let meta = CommitMeta {
                sha: record.sha,
                summary: record.summary,
                author: record.author,
                author_email: record.author_email,
                author_time_unix: record.author_time_unix,
                files,
            };
            writer.put_commit_meta(ord, &meta)?;
            writer.put_ord_for_sha(&sha20, ord)?;
            reach.insert(ord);
            written += 1;
        }
    }
    Ok(written)
}

/// How many commits one rayon task decodes through a single gix repository handle. See
/// [`Repo::commit_records`]: a handle per commit meant gix's delta-base cache never got a second
/// look at anything. With `RECORD_CHUNK`/`RECORD_BATCH` = 64 tasks per chunk the pool still
/// saturates, while live handles — and their caches — stay bounded by the worker count.
const RECORD_BATCH: usize = 128;

/// Compute a chunk of commits' full records in parallel. Each rayon task gets its own thread-local
/// gix repository via `Repo::commit_records` → `Repo::local`, so the `!Sync` gix repo is never
/// shared. rayon's `collect` is order-preserving, so records still line up with `chrono`.
fn compute_records(repo: &Repo, chrono: &[&String]) -> Vec<Option<crate::git::CommitInfo>> {
    chrono
        .par_chunks(RECORD_BATCH)
        .flat_map(|batch| {
            let shas: Vec<&str> = batch.iter().map(|sha| sha.as_str()).collect();
            repo.commit_records(&shas)
        })
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

    fn intern(&mut self, rel: &RelPath, writer: &mut super::GitHistoryWriter) -> Result<u32, GitHistoryError> {
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

#[cfg(test)]
#[path = "builder_tests.rs"]
mod tests;
