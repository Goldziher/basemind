//! Build, incrementally append to, and revalidate the git-history index.
//!
//! Parity guarantee: the index is built from [`Repo::commit_record`], which returns the same
//! exact-path, no-rename-follow relation the live history tools filter on (a merge lists only the
//! files it changed against every parent, as `git log --cc` does), so an indexed query result is
//! identical to the live walk it replaces.

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
    /// The index was left untouched because this checkout cannot advance it without a wipe: a linked
    /// worktree whose HEAD does not descend from the indexed head (`reason` says why). Tools read via
    /// the live walk at this head; the main checkout owns the index.
    Skipped { reason: &'static str },
    /// Wiped and rebuilt from scratch, indexing `commits` commits. `reason` is `"initial"` (nothing
    /// was indexed), `"interrupted"` (a build did not finish), `"deepened"` (the shallow clone it was
    /// built in has been deepened or unshallowed) or `"history-rewrite"` (HEAD no longer descends from
    /// the indexed history).
    FullRebuild { reason: &'static str, commits: u32 },
}

/// Bring the index up to date with `repo`'s current HEAD, running the revalidation decision tree.
/// Best-effort callers ignore the error; the index simply stays stale and tools fall back to the
/// live walk until the next successful sync.
/// Only the process that HOLDS the fjall database may build it. Syncs of one database serialize on
/// its build lock, which every clone of the handle shares: callers that queue behind a sync find the
/// index fresh and return at once.
///
/// And only a repository the caller actually rooted at is built: `repo` reaching here from a
/// workspace root *below* its workdir means an ancestor repository was inherited, which
/// [`history_scope_ok`](super::history_scope_ok) refuses.
pub fn sync(index: &GitHistoryIndex, repo: &Repo, _basemind_dir: &Path) -> Result<RebuildOutcome, GitHistoryError> {
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
    match history_pool() {
        Some(pool) => pool.install(|| sync_inner(index, repo)),
        None => sync_inner(index, repo),
    }
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

fn sync_inner(index: &GitHistoryIndex, repo: &Repo) -> Result<RebuildOutcome, GitHistoryError> {
    if !super::history_scope_ok(repo) {
        return Err(GitHistoryError::ForeignHistory {
            root: repo.origin().to_path_buf(),
            workdir: repo.workdir().to_path_buf(),
        });
    }
    let head = match repo.resolve_rev("HEAD") {
        Ok(h) => h,
        Err(_) => return Ok(RebuildOutcome::Fresh),
    };

    let deepened = root_deepened(index, repo);
    if !deepened && index.last_indexed_head_hex().as_deref() == Some(head.as_str()) {
        return Ok(RebuildOutcome::Fresh);
    }

    // A descendant of the indexed head is always safe to append, from any worktree.
    if !deepened
        && let Some(last_hex) = index.last_indexed_head_hex()
        && repo.has_commit(&last_hex)
        && repo.is_ancestor(&last_hex, &head)
        && fingerprint_ok(index, repo, &head)
    {
        return append_since(index, repo, &last_hex, &head).inspect_err(|_| record_failure(index, &head));
    }

    // The index tracks ONE head, and every worktree of a clone shares it. A linked worktree on its
    // own branch is usually NOT a descendant of the head another worktree indexed, so letting it
    // fall through to the wipe below rebuilds the whole multi-100k-commit history (multi-GB, minutes)
    // on every switch between worktrees, and the main checkout then wipes it back. Only the first
    // build is left to a linked worktree; afterwards it may append (above) but never wipe. The index
    // is a pure accelerator (tools use it only when `last_indexed_head == HEAD`), so a linked
    // worktree at any other head reads via the live walk: correct, just slower.
    // An interrupted index makes `is_empty` true, so a linked worktree may rebuild it here and the
    // main checkout then rebuild it again for its own head ("history-rewrite"): the ping-pong above,
    // once per interruption. ~keep
    if repo.is_linked_worktree() && !index.is_empty() {
        return Ok(RebuildOutcome::Skipped {
            reason: "linked worktree diverges from the indexed head",
        });
    }

    let interrupted = index.build_interrupted();
    if interrupted && backing_off(index, &head, now_unix()) {
        return Ok(RebuildOutcome::Skipped {
            reason: "a build at this head failed within the hour",
        });
    }
    let reason = if interrupted {
        "interrupted"
    } else if index.is_empty() {
        "initial"
    } else if deepened {
        "deepened"
    } else {
        "history-rewrite"
    };
    index.clear()?;
    rebuild(index, repo, reason).inspect_err(|_| record_failure(index, &head))
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

fn record_failure(index: &GitHistoryIndex, head: &str) {
    if let Some(head20) = keys::sha_hex_to_raw(head) {
        index.record_failed_build(&head20, now_unix());
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
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
fn rebuild(index: &GitHistoryIndex, repo: &Repo, reason: &'static str) -> Result<RebuildOutcome, GitHistoryError> {
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
    let mut writer = index.writer()?;
    writer.begin()?;
    writer.put_shallow_boundary(&repo.shallow_boundary())?;

    let written = fold_chunked(
        index,
        repo,
        &chrono,
        0,
        false,
        &mut interner,
        &mut postings,
        &mut term_postings,
        &mut writer,
    )?;
    for (path_id, ords) in postings {
        writer.put_posting(path_id, &encoding::encode_ords(&ords))?;
    }
    for (term_key, ords) in term_postings {
        writer.put_term_posting(&term_key, &encoding::encode_ords(&ords))?;
    }
    writer.finish_meta(&head20, &root20, total, interner.next_path_id, written)?;
    index.local().compact()?;
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
        let writer = index.writer()?;
        writer.finish_meta(&head20, &root20, start_ord, index.next_path_id(), index.commit_count())?;
        return Ok(RebuildOutcome::Incremental { added: 0 });
    }

    let chrono: Vec<&String> = new_newest_first.iter().rev().collect();
    let mut interner = PathInterner::new(index, index.next_path_id());
    let mut postings: AHashMap<u32, Vec<u32>> = AHashMap::new();
    let mut term_postings: AHashMap<Vec<u8>, Vec<u32>> = AHashMap::new();
    let mut writer = index.writer()?;
    writer.begin()?;

    let added = fold_chunked(
        index,
        repo,
        &chrono,
        start_ord,
        true,
        &mut interner,
        &mut postings,
        &mut term_postings,
        &mut writer,
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
/// absolute position in `chrono` (`start_ord` plus the global offset) so a `None`/unparsable record
/// still consumes its slot, preserving the positional ordinal contract the non-chunked fold had.
/// Returns the number of commits actually written. `dedup` skips commits already present in
/// `gh_ord_by_sha` (the incremental append's defensive guard). It never meets a row a cut-off run
/// left, because such a run leaves the index marked in progress and the next sync rebuilds it.
#[allow(clippy::too_many_arguments)]
fn fold_chunked(
    index: &GitHistoryIndex,
    repo: &Repo,
    chrono: &[&String],
    start_ord: u32,
    dedup: bool,
    interner: &mut PathInterner,
    postings: &mut AHashMap<u32, Vec<u32>>,
    term_postings: &mut AHashMap<Vec<u8>, Vec<u32>>,
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
                continue;
            }
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
mod tests {
    use std::process::Command;

    use super::*;

    fn git(root: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@e.x")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@e.x")
            .output()
            .expect("git in PATH");
        assert!(out.status.success(), "git {args:?} failed");
        String::from_utf8(out.stdout).expect("utf8")
    }

    fn commit(root: &Path, path: &str, content: &str, msg: &str) {
        std::fs::write(root.join(path), content).expect("write the file");
        git(root, &["add", path]);
        git(root, &["commit", "-qm", msg]);
    }

    fn touching(index: &GitHistoryIndex, path: &str) -> Vec<String> {
        index
            .commits_touching(&RelPath::from(path.as_bytes()), 0, 100)
            .expect("read the posting list")
            .into_iter()
            .map(|c| c.sha)
            .collect()
    }

    /// A sync killed after the writer's first batch commit leaves the new commits' rows durable and
    /// their postings and head not. The retry used to skip every commit it found a row for, so they
    /// never reached a posting list and the index then claimed to be fresh.
    #[test]
    fn an_append_cut_off_after_a_batch_commit_is_rebuilt_not_skipped() {
        let repo_dir = tempfile::tempdir().expect("tempdir");
        let root = repo_dir.path();
        git(root, &["init", "-q"]);
        git(root, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        commit(root, "a.rs", "1", "c1");
        let cache = tempfile::tempdir().expect("cache dir");
        let index = GitHistoryIndex::open(cache.path()).expect("open the index");
        let repo = Repo::discover(root).expect("discover");
        sync(&index, &repo, cache.path()).expect("first build");

        commit(root, "a.rs", "2", "c2");
        commit(root, "a.rs", "3", "c3");
        let repo = Repo::discover(root).expect("rediscover");
        let last = index.last_indexed_head_hex().expect("an indexed head");
        let new_shas = repo.new_commit_shas(&last).expect("new commits");
        assert_eq!(new_shas.len(), 2);
        let chrono: Vec<&String> = new_shas.iter().rev().collect();
        {
            let mut writer = index.writer().expect("writer");
            writer.begin().expect("mark the append");
            let mut interner = PathInterner::new(&index, index.next_path_id());
            let mut postings = AHashMap::new();
            let mut term_postings = AHashMap::new();
            fold_chunked(
                &index,
                &repo,
                &chrono,
                index.next_ord(),
                true,
                &mut interner,
                &mut postings,
                &mut term_postings,
                &mut writer,
            )
            .expect("fold the new commits");
            writer.flush().expect("commit the batch");
        }
        assert_eq!(
            index.last_indexed_head_hex(),
            None,
            "a cut-off append is never served as fresh"
        );

        let outcome = sync(&index, &repo, cache.path()).expect("resync");
        assert_eq!(
            outcome,
            RebuildOutcome::FullRebuild {
                reason: "interrupted",
                commits: 3
            }
        );
        let head = git(root, &["rev-parse", "HEAD"]);
        assert_eq!(index.last_indexed_head_hex().as_deref(), Some(head.trim()));
        let expected: Vec<String> = git(root, &["log", "--full-history", "--format=%H", "--", "a.rs"])
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(expected.len(), 3);
        assert_eq!(
            touching(&index, "a.rs"),
            expected,
            "every commit is in the posting list"
        );
    }

    /// A build that fails at a commit it cannot read fails again at that commit: a sync at the same
    /// HEAD leaves the marked index to the live walk for an hour instead of walking the whole
    /// history each time, then tries again.
    #[test]
    fn a_failed_build_is_not_retried_at_the_same_head_within_the_hour() {
        let repo_dir = tempfile::tempdir().expect("tempdir");
        let root = repo_dir.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        commit(root, "a.rs", "1", "c1");
        let cache = tempfile::tempdir().expect("cache dir");
        let index = GitHistoryIndex::open(cache.path()).expect("open the index");
        let repo = Repo::discover(root).expect("discover");
        let head = git(root, &["rev-parse", "HEAD"]);
        let head20 = keys::sha_hex_to_raw(head.trim()).expect("a sha");
        index
            .writer()
            .expect("writer")
            .begin()
            .expect("a build that was cut off");
        index.record_failed_build(&head20, now_unix());

        let outcome = sync(&index, &repo, cache.path()).expect("sync");
        assert!(matches!(outcome, RebuildOutcome::Skipped { .. }), "{outcome:?}");
        assert!(index.build_interrupted(), "the marked index is left as it was");

        index.record_failed_build(&head20, now_unix() - FAILED_BUILD_BACKOFF_SECS);
        let outcome = sync(&index, &repo, cache.path()).expect("sync");
        assert_eq!(
            outcome,
            RebuildOutcome::FullRebuild {
                reason: "interrupted",
                commits: 1
            }
        );
        assert_eq!(index.failed_build_at(&head20), None, "a stamped head clears the record");
    }

    /// An append cut off after a batch commit left a new path's id rows durable while the next free
    /// id was never advanced. A retry that reused the id gave it to another path, whose history then
    /// served the first path's commits. The cut-off index is rebuilt instead, so no id names two
    /// paths and each path's history is its own.
    #[test]
    fn a_retry_after_a_cut_off_append_never_gives_one_path_id_to_two_paths() {
        let repo_dir = tempfile::tempdir().expect("tempdir");
        let root = repo_dir.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        commit(root, "a.rs", "1", "c1");
        let cache = tempfile::tempdir().expect("cache dir");
        let index = GitHistoryIndex::open(cache.path()).expect("open the index");
        sync(&index, &Repo::discover(root).expect("discover"), cache.path()).expect("first build");

        commit(root, "b.rs", "1", "adds b");
        let repo = Repo::discover(root).expect("rediscover");
        let last = index.last_indexed_head_hex().expect("an indexed head");
        let new_shas = repo.new_commit_shas(&last).expect("new commits");
        let chrono: Vec<&String> = new_shas.iter().rev().collect();
        {
            let mut writer = index.writer().expect("writer");
            writer.begin().expect("mark the append");
            let mut interner = PathInterner::new(&index, index.next_path_id());
            let (mut postings, mut term_postings) = (AHashMap::new(), AHashMap::new());
            fold_chunked(
                &index,
                &repo,
                &chrono,
                index.next_ord(),
                true,
                &mut interner,
                &mut postings,
                &mut term_postings,
                &mut writer,
            )
            .expect("fold");
            writer.flush().expect("commit the batch: b.rs's id rows are durable");
        }
        commit(root, "c.rs", "1", "adds c");
        commit(root, "b.rs", "2", "edits b");
        let repo = Repo::discover(root).expect("rediscover");

        sync(&index, &repo, cache.path()).expect("retry");

        let local = index.local();
        let paths: Vec<RelPath> = (0..index.next_path_id())
            .map(|id| local.path_for_id(id).expect("every id names a path"))
            .collect();
        let distinct: ahash::AHashSet<&RelPath> = paths.iter().collect();
        assert_eq!(distinct.len(), paths.len(), "no id names two paths: {paths:?}");
        for path in ["a.rs", "b.rs", "c.rs"] {
            let expected: Vec<String> = git(root, &["log", "--full-history", "--format=%H", "--", path])
                .lines()
                .map(str::to_owned)
                .collect();
            assert_eq!(touching(&index, path), expected, "{path}'s history is its own");
        }
    }

    /// Every table file of the git-history database under `cache`, by path.
    fn table_files(cache: &Path) -> Vec<std::path::PathBuf> {
        fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("read the index dir").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.parent().and_then(Path::file_name) == Some("tables".as_ref()) {
                    out.push(path);
                }
            }
        }
        let mut out = Vec::new();
        walk(&cache.join(super::super::GIT_HISTORY_DIR), &mut out);
        out.sort();
        out
    }

    /// A one-commit append used to flush and major-compact all eight keyspaces, rewriting every
    /// table of the history (seconds on a deep repository). It now leaves the tables as they were,
    /// and the appended commit is still indexed after a reopen.
    #[test]
    fn a_one_commit_append_rewrites_no_table() {
        let repo_dir = tempfile::tempdir().expect("tempdir");
        let root = repo_dir.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        commit(root, "a.rs", "1", "c1");
        commit(root, "b.rs", "1", "c2");
        let cache = tempfile::tempdir().expect("cache dir");
        let index = GitHistoryIndex::open(cache.path()).expect("open the index");
        sync(&index, &Repo::discover(root).expect("discover"), cache.path()).expect("first build");
        let built = table_files(cache.path());
        assert!(
            built.len() >= 7,
            "the build compacted each written keyspace to a table: {built:?}"
        );

        commit(root, "a.rs", "2", "c3");
        let outcome = sync(&index, &Repo::discover(root).expect("rediscover"), cache.path()).expect("append");
        assert_eq!(outcome, RebuildOutcome::Incremental { added: 1 });
        assert_eq!(
            table_files(cache.path()),
            built,
            "the append flushed or compacted a table"
        );

        drop(index);
        let index = GitHistoryIndex::open(cache.path()).expect("reopen the index");
        let head = git(root, &["rev-parse", "HEAD"]);
        assert_eq!(index.last_indexed_head_hex().as_deref(), Some(head.trim()));
        let expected: Vec<String> = git(root, &["log", "--format=%H", "--", "a.rs"])
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(touching(&index, "a.rs"), expected);
    }
}
