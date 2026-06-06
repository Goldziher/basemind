//! Thin gitoxide wrapper.
//!
//! Everything that touches `gix` lives in this module. The rest of the crate sees plain
//! `String` / `PathBuf` / small structs so we can swap the underlying git library later
//! without rewriting half of the codebase.

use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitError {
    #[error("not inside a git repository (starting from {0})")]
    NotARepo(PathBuf),
    #[error("git discover error: {0}")]
    Discover(String),
    #[error("rev parse failed for {rev}: {msg}")]
    RevParse { rev: String, msg: String },
    #[error("git error reading {what}: {msg}")]
    Read { what: String, msg: String },
    #[error("git error: {0}")]
    Other(String),
}

/// Thread-safe wrapper around a gix repository.
///
/// `gix::Repository` is `!Sync` (it has interior `RefCell`s for object/index caches). gix
/// expects each thread to own its own `Repository`, obtained via `.to_thread_local()` on a
/// `ThreadSafeRepository`. We hold the thread-safe form here and freshen a per-call
/// `Repository` inside each method — that lets us pass `&Repo` across rayon thread
/// boundaries without `Send`/`Sync` errors.
pub struct Repo {
    inner: gix::ThreadSafeRepository,
    workdir: PathBuf,
}

#[derive(Debug, Default, Clone)]
pub struct WorkingTreeStatus {
    pub staged_added: Vec<String>,
    pub staged_modified: Vec<String>,
    pub staged_deleted: Vec<String>,
    pub modified: Vec<String>,
    pub untracked: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
}

impl ChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeKind::Added => "added",
            ChangeKind::Modified => "modified",
            ChangeKind::Deleted => "deleted",
            ChangeKind::Renamed => "renamed",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CommitInfo {
    pub sha: String,
    pub short_sha: String,
    pub summary: String,
    pub author: String,
    pub author_time_unix: i64,
    /// Files that changed in this commit relative to its first parent.
    /// Empty for the root commit, and empty when `include_files=false` was used.
    pub files: Vec<(String, ChangeKind)>,
}

/// One contiguous diff hunk between two file revisions. Line counts are 1-based.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Hunk {
    pub kind: HunkKind,
    pub old_line_start: u32,
    pub old_line_count: u32,
    pub new_line_start: u32,
    pub new_line_count: u32,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HunkKind {
    Added,
    Removed,
    Modified,
}

impl HunkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            HunkKind::Added => "added",
            HunkKind::Removed => "removed",
            HunkKind::Modified => "modified",
        }
    }
}

/// A single blame hunk: a run of consecutive lines all introduced by one commit.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BlameHunk {
    pub commit_sha: String,
    pub short_sha: String,
    /// 1-based line where the hunk starts in the blamed file.
    pub start_line: u32,
    /// Number of lines covered by this hunk (>= 1).
    pub len: u32,
    /// 1-based line where the hunk starts in the source commit (before any renames/offsets).
    pub source_start_line: u32,
    pub author: String,
    pub author_time_unix: i64,
    pub summary: String,
    /// Set when the file was renamed at `commit_sha` — name in the commit's tree.
    pub source_path: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BlameResult {
    pub path: String,
    pub suspect_sha: String,
    pub hunks: Vec<BlameHunk>,
}

#[derive(Debug, Clone)]
pub struct RepoInfo {
    pub workdir: PathBuf,
    pub head_sha: Option<String>,
    pub head_short_sha: Option<String>,
    pub branch: Option<String>,
}

impl Repo {
    /// Walk up from `start` looking for `.git`. Returns `NotARepo` if discovery fails.
    pub fn discover(start: &Path) -> Result<Self, GitError> {
        let inner = gix::ThreadSafeRepository::discover(start)
            .map_err(|_| GitError::NotARepo(start.to_path_buf()))?;
        let workdir = inner
            .work_dir()
            .ok_or_else(|| GitError::Other("bare repositories are not supported".to_string()))?
            .to_path_buf();
        Ok(Self { inner, workdir })
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// Borrow a per-thread `Repository` view of the wrapped thread-safe repo.
    fn local(&self) -> gix::Repository {
        self.inner.to_thread_local()
    }

    /// Resolve any rev-spec (HEAD, branch name, partial sha, HEAD~3) to a 40-hex sha.
    pub fn resolve_rev(&self, rev: &str) -> Result<String, GitError> {
        let local = self.local();
        let id = local
            .rev_parse_single(rev)
            .map_err(|e| GitError::RevParse {
                rev: rev.to_string(),
                msg: e.to_string(),
            })?;
        Ok(id.to_string())
    }

    /// List forward-slash relative paths of every blob recorded in the staging index.
    pub fn list_paths_staged(&self) -> Result<Vec<String>, GitError> {
        let local = self.local();
        let index = local.index().map_err(|e| GitError::Read {
            what: "git index".to_string(),
            msg: e.to_string(),
        })?;
        let mut out = Vec::with_capacity(index.entries().len());
        for entry in index.entries() {
            let path = entry.path(&index);
            if let Ok(s) = std::str::from_utf8(path) {
                out.push(s.to_string());
            }
        }
        Ok(out)
    }

    /// List forward-slash relative paths of every blob in the tree at `rev_sha`.
    pub fn list_paths_rev(&self, rev_sha: &str) -> Result<Vec<String>, GitError> {
        let local = self.local();
        let tree = resolve_tree(&local, rev_sha)?;
        let recorder = tree
            .traverse()
            .breadthfirst
            .files()
            .map_err(|e| GitError::Read {
                what: format!("tree {rev_sha}"),
                msg: e.to_string(),
            })?;
        let mut out = Vec::with_capacity(recorder.len());
        for entry in recorder {
            if let Ok(s) = std::str::from_utf8(&entry.filepath) {
                out.push(s.to_string());
            }
        }
        Ok(out)
    }

    /// Read the staged blob content for `rel` (forward-slash). `None` = not in the index.
    pub fn read_blob_staged(&self, rel: &str) -> Result<Option<Vec<u8>>, GitError> {
        let local = self.local();
        let index = local.index().map_err(|e| GitError::Read {
            what: "git index".to_string(),
            msg: e.to_string(),
        })?;
        let needle = rel.as_bytes();
        for entry in index.entries() {
            if entry.path(&index) == needle {
                let object = local.find_object(entry.id).map_err(|e| GitError::Read {
                    what: format!("blob {rel}"),
                    msg: e.to_string(),
                })?;
                return Ok(Some(object.data.clone()));
            }
        }
        Ok(None)
    }

    /// Read the blob content for `rel` at `rev_sha`. `None` = path doesn't exist at that rev.
    pub fn read_blob_at_rev(&self, rev_sha: &str, rel: &str) -> Result<Option<Vec<u8>>, GitError> {
        let local = self.local();
        let tree = resolve_tree(&local, rev_sha)?;
        let entry = match tree.lookup_entry_by_path(rel).map_err(|e| GitError::Read {
            what: format!("{rev_sha}:{rel}"),
            msg: e.to_string(),
        })? {
            Some(e) => e,
            None => return Ok(None),
        };
        let object = entry.object().map_err(|e| GitError::Read {
            what: format!("{rev_sha}:{rel}"),
            msg: e.to_string(),
        })?;
        let blob = object.try_into_blob().map_err(|e| GitError::Read {
            what: format!("{rev_sha}:{rel}"),
            msg: format!("not a blob: {e}"),
        })?;
        Ok(Some(blob.data.clone()))
    }

    /// Porcelain-style status: bucketed by stage vs working tree, including untracked files.
    pub fn status_porcelain(&self) -> Result<WorkingTreeStatus, GitError> {
        let local = self.local();
        let mut out = WorkingTreeStatus::default();

        let platform = local
            .status(gix::progress::Discard)
            .map_err(|e| GitError::Read {
                what: "status".to_string(),
                msg: e.to_string(),
            })?;
        let iter = platform.into_iter(None).map_err(|e| GitError::Read {
            what: "status".to_string(),
            msg: e.to_string(),
        })?;
        for item in iter {
            let item = match item {
                Ok(i) => i,
                Err(_) => continue,
            };
            classify_status_item(&item, &mut out);
        }
        Ok(out)
    }

    /// Recent commits across the repo, newest first.
    pub fn log_paths(
        &self,
        max_commits: usize,
        include_files: bool,
    ) -> Result<Vec<CommitInfo>, GitError> {
        let local = self.local();
        let head = local.head_id().map_err(|e| GitError::Read {
            what: "HEAD".to_string(),
            msg: e.to_string(),
        })?;
        let walk = local
            .rev_walk([head])
            .sorting(gix::revision::walk::Sorting::ByCommitTime(
                gix::traverse::commit::simple::CommitTimeOrder::NewestFirst,
            ))
            .all()
            .map_err(|e| GitError::Read {
                what: "rev walk".to_string(),
                msg: e.to_string(),
            })?;
        let mut out = Vec::with_capacity(max_commits.min(1024));
        for info in walk.take(max_commits) {
            let Ok(info) = info else { continue };
            if let Some(ci) = build_commit_info(&local, info.id, include_files) {
                out.push(ci);
            }
        }
        Ok(out)
    }

    /// Recent commits that touch `rel`, newest first.
    pub fn log_for_path(&self, rel: &str, max_commits: usize) -> Result<Vec<CommitInfo>, GitError> {
        let local = self.local();
        let head = local.head_id().map_err(|e| GitError::Read {
            what: "HEAD".to_string(),
            msg: e.to_string(),
        })?;
        let walk = local
            .rev_walk([head])
            .sorting(gix::revision::walk::Sorting::ByCommitTime(
                gix::traverse::commit::simple::CommitTimeOrder::NewestFirst,
            ))
            .all()
            .map_err(|e| GitError::Read {
                what: "rev walk".to_string(),
                msg: e.to_string(),
            })?;
        let mut out = Vec::new();
        for info in walk {
            if out.len() >= max_commits {
                break;
            }
            let Ok(info) = info else { continue };
            if !commit_touches_path(&local, info.id, rel) {
                continue;
            }
            if let Some(ci) = build_commit_info(&local, info.id, false) {
                out.push(ci);
            }
        }
        Ok(out)
    }

    /// Blame the given file at the given suspect rev, optionally clamped to a 1-based
    /// inclusive line range. Returns one [`BlameHunk`] per consecutive run of lines that
    /// share a source commit. Author info is fetched once per unique commit to keep cost
    /// linear in the number of distinct commits, not in the number of hunks.
    pub fn blame_file(
        &self,
        suspect_sha: &str,
        path: &str,
        line_range: Option<(u32, u32)>,
    ) -> Result<BlameResult, GitError> {
        use gix::bstr::BStr;

        let local = self.local();
        let suspect = local
            .rev_parse_single(suspect_sha)
            .map_err(|e| GitError::RevParse {
                rev: suspect_sha.to_string(),
                msg: e.to_string(),
            })?
            .detach();

        let mut options = gix::repository::blame_file::Options {
            diff_algorithm: Some(gix::diff::blob::Algorithm::Histogram),
            ranges: gix::blame::BlameRanges::default(),
            since: None,
            rewrites: Some(gix::diff::Rewrites::default()),
        };
        if let Some((lo, hi)) = line_range {
            options.ranges = gix::blame::BlameRanges::from_one_based_inclusive_range(lo..=hi)
                .map_err(|e| GitError::Other(format!("invalid blame range {lo}..={hi}: {e}")))?;
        }

        let outcome = local
            .blame_file(BStr::new(path.as_bytes()), suspect, options)
            .map_err(|e| GitError::Read {
                what: format!("blame {suspect_sha}:{path}"),
                msg: e.to_string(),
            })?;

        // Resolve author info per unique commit id, then map each hunk.
        let mut author_cache: ahash::AHashMap<gix::ObjectId, (String, i64, String)> =
            ahash::AHashMap::new();
        let mut hunks = Vec::with_capacity(outcome.entries.len());
        for entry in &outcome.entries {
            let (author, time, summary) = author_cache
                .entry(entry.commit_id)
                .or_insert_with(|| {
                    let commit = match local.find_commit(entry.commit_id) {
                        Ok(c) => c,
                        Err(_) => return ("?".to_string(), 0, String::new()),
                    };
                    let author = commit
                        .author()
                        .ok()
                        .and_then(|a| std::str::from_utf8(a.name).ok().map(|s| s.to_string()))
                        .unwrap_or_else(|| "?".to_string());
                    let time = commit
                        .author()
                        .ok()
                        .and_then(|a| a.time().ok())
                        .map(|t| t.seconds)
                        .unwrap_or(0);
                    let summary = commit
                        .message()
                        .ok()
                        .map(|m| m.summary().to_string())
                        .unwrap_or_default();
                    (author, time, summary)
                })
                .clone();
            let sha = entry.commit_id.to_string();
            let short_sha = sha[..7.min(sha.len())].to_string();
            let source_path = entry
                .source_file_name
                .as_ref()
                .and_then(|b| std::str::from_utf8(b).ok().map(|s| s.to_string()));
            hunks.push(BlameHunk {
                commit_sha: sha,
                short_sha,
                start_line: entry.start_in_blamed_file + 1,
                len: entry.len.get(),
                source_start_line: entry.start_in_source_file + 1,
                author,
                author_time_unix: time,
                summary,
                source_path,
            });
        }
        hunks.sort_by_key(|h| h.start_line);

        Ok(BlameResult {
            path: path.to_string(),
            suspect_sha: suspect_sha.to_string(),
            hunks,
        })
    }

    /// Content-level hunks for `path` between `rev_old` and `rev_new`. Returns the bytes for
    /// either side and the unified-diff hunks. The output is empty (an empty Vec, not an
    /// error) when the file exists at both revs but the bytes are identical, and `None` when
    /// the path is absent at both sides.
    pub fn diff_file(
        &self,
        rev_old: &str,
        rev_new: &str,
        path: &str,
    ) -> Result<Option<(Vec<Hunk>, bool, bool)>, GitError> {
        let old_bytes = self.read_blob_at_rev(rev_old, path)?;
        let new_bytes = self.read_blob_at_rev(rev_new, path)?;
        if old_bytes.is_none() && new_bytes.is_none() {
            return Ok(None);
        }
        let old_buf = old_bytes.clone().unwrap_or_default();
        let new_buf = new_bytes.clone().unwrap_or_default();
        let hunks = compute_hunks(&old_buf, &new_buf);
        Ok(Some((hunks, old_bytes.is_some(), new_bytes.is_some())))
    }

    /// Diff the commit at `commit_sha` against its first parent. Returns the per-file
    /// (path, change-kind) list. **No caching** — call through `GitCache::commit_files`
    /// in hot paths. Public so the cache layer can drive it.
    pub fn commit_files_uncached(
        &self,
        commit_sha: &str,
    ) -> Result<Vec<(String, ChangeKind)>, GitError> {
        let local = self.local();
        let id = local
            .rev_parse_single(commit_sha)
            .map_err(|e| GitError::RevParse {
                rev: commit_sha.to_string(),
                msg: e.to_string(),
            })?
            .detach();
        Ok(commit_files(&local, id).unwrap_or_default())
    }

    pub fn info(&self) -> Result<RepoInfo, GitError> {
        let local = self.local();
        let head_id = local.head_id().ok();
        let head_sha = head_id.as_ref().map(|id| id.to_string());
        let head_short_sha = head_sha.as_ref().map(|s| s[..7.min(s.len())].to_string());
        let branch = local
            .head_name()
            .ok()
            .flatten()
            .and_then(|n| std::str::from_utf8(n.shorten()).ok().map(|s| s.to_string()));
        Ok(RepoInfo {
            workdir: self.workdir.clone(),
            head_sha,
            head_short_sha,
            branch,
        })
    }
}

// ─── module-level helpers — operate on a per-thread Repository ──────────────────

fn resolve_tree<'r>(local: &'r gix::Repository, rev_sha: &str) -> Result<gix::Tree<'r>, GitError> {
    let object = local
        .rev_parse_single(rev_sha)
        .map_err(|e| GitError::RevParse {
            rev: rev_sha.to_string(),
            msg: e.to_string(),
        })?
        .object()
        .map_err(|e| GitError::Read {
            what: rev_sha.to_string(),
            msg: e.to_string(),
        })?;
    // Try to peel a commit to its tree first; if it was already a tree, take it as-is.
    let kind = object.kind;
    match kind {
        gix::object::Kind::Commit => object.try_into_commit().ok().and_then(|c| c.tree().ok()),
        gix::object::Kind::Tree => object.try_into_tree().ok(),
        _ => None,
    }
    .ok_or_else(|| GitError::Read {
        what: rev_sha.to_string(),
        msg: "not a commit or tree".to_string(),
    })
}

fn build_commit_info(
    local: &gix::Repository,
    id: gix::ObjectId,
    include_files: bool,
) -> Option<CommitInfo> {
    let commit = local.find_commit(id).ok()?;
    let sha = id.to_string();
    let short_sha = sha[..7.min(sha.len())].to_string();
    let summary = commit
        .message()
        .ok()
        .map(|m| m.summary().to_string())
        .unwrap_or_default();
    let author_ref = commit.author().ok()?;
    let author = std::str::from_utf8(author_ref.name)
        .unwrap_or("?")
        .to_string();
    let author_time_unix = author_ref.time().ok().map(|t| t.seconds).unwrap_or(0);

    let files = if include_files {
        commit_files(local, id).unwrap_or_default()
    } else {
        Vec::new()
    };
    Some(CommitInfo {
        sha,
        short_sha,
        summary,
        author,
        author_time_unix,
        files,
    })
}

/// Diff `commit_id`'s tree against its first parent and return (path, change-kind) pairs.
fn commit_files(
    local: &gix::Repository,
    commit_id: gix::ObjectId,
) -> Option<Vec<(String, ChangeKind)>> {
    let commit = local.find_commit(commit_id).ok()?;
    let tree = commit.tree().ok()?;
    let parent_tree = commit
        .parent_ids()
        .next()
        .and_then(|pid| local.find_commit(pid).ok())
        .and_then(|c| c.tree().ok());
    let Some(parent_tree) = parent_tree else {
        // Initial commit — every entry is "added".
        let mut recorder = tree.traverse().breadthfirst.files().ok()?;
        recorder.sort_by(|a, b| a.filepath.cmp(&b.filepath));
        let mut out = Vec::with_capacity(recorder.len());
        for e in recorder {
            if let Ok(p) = std::str::from_utf8(&e.filepath) {
                out.push((p.to_string(), ChangeKind::Added));
            }
        }
        return Some(out);
    };

    // `for_each_to_obtain_tree(other, ..)` walks the changes needed to convert `self` →
    // `other`. To describe commit-relative-to-parent we run it from parent → commit.
    let mut out: Vec<(String, ChangeKind)> = Vec::new();
    let mut platform = parent_tree.changes().ok()?;
    platform
        .for_each_to_obtain_tree(&tree, |change| {
            if let Some((path, kind)) = classify_tree_change(&change) {
                out.push((path, kind));
            }
            Ok::<_, std::convert::Infallible>(gix::object::tree::diff::Action::Continue(()))
        })
        .ok()?;
    Some(out)
}

fn commit_touches_path(local: &gix::Repository, commit_id: gix::ObjectId, rel: &str) -> bool {
    let Some(files) = commit_files(local, commit_id) else {
        return false;
    };
    files.iter().any(|(p, _)| p == rel)
}

/// Line-diff between two byte buffers using the histogram algorithm + slider heuristics.
/// Returns one `Hunk` per `imara_diff::Hunk` — runs without surrounding context lines,
/// because agents have direct access to the source via the outline tools.
fn compute_hunks(old: &[u8], new: &[u8]) -> Vec<Hunk> {
    use gix::diff::blob::{Algorithm, InternedInput, diff_with_slider_heuristics, sources};

    let input = InternedInput::new(sources::byte_lines(old), sources::byte_lines(new));
    let diff = diff_with_slider_heuristics(Algorithm::Histogram, &input);

    let old_lines = line_byte_offsets(old);
    let new_lines = line_byte_offsets(new);
    let mut out =
        Vec::with_capacity(diff.count_additions() as usize + diff.count_removals() as usize);
    for hunk in diff.hunks() {
        let removed_count = hunk.before.end - hunk.before.start;
        let added_count = hunk.after.end - hunk.after.start;
        let kind = match (removed_count, added_count) {
            (0, _) => HunkKind::Added,
            (_, 0) => HunkKind::Removed,
            _ => HunkKind::Modified,
        };
        let mut removed = String::new();
        for line in hunk.before.start..hunk.before.end {
            if let Some((s, e)) = old_lines.get(line as usize).copied()
                && let Ok(t) = std::str::from_utf8(&old[s as usize..e as usize])
            {
                removed.push_str(t);
            }
        }
        let mut added = String::new();
        for line in hunk.after.start..hunk.after.end {
            if let Some((s, e)) = new_lines.get(line as usize).copied()
                && let Ok(t) = std::str::from_utf8(&new[s as usize..e as usize])
            {
                added.push_str(t);
            }
        }
        let text = if removed_count == 0 {
            added
        } else if added_count == 0 {
            removed
        } else {
            // Git-style unified body: lines from the removed side prefixed with '-',
            // added side with '+'. Trailing newlines preserved when present in source.
            let mut s = String::with_capacity(removed.len() + added.len());
            for line in removed.lines() {
                s.push('-');
                s.push_str(line);
                s.push('\n');
            }
            for line in added.lines() {
                s.push('+');
                s.push_str(line);
                s.push('\n');
            }
            s
        };
        out.push(Hunk {
            kind,
            old_line_start: hunk.before.start + 1,
            old_line_count: removed_count,
            new_line_start: hunk.after.start + 1,
            new_line_count: added_count,
            text,
        });
    }
    out
}

fn line_byte_offsets(buf: &[u8]) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let mut s: u32 = 0;
    for (i, &b) in buf.iter().enumerate() {
        if b == b'\n' {
            out.push((s, (i + 1) as u32));
            s = (i + 1) as u32;
        }
    }
    if (s as usize) < buf.len() {
        out.push((s, buf.len() as u32));
    }
    out
}

fn classify_tree_change(
    change: &gix::object::tree::diff::Change<'_, '_, '_>,
) -> Option<(String, ChangeKind)> {
    use gix::object::tree::diff::Change::*;
    match change {
        Addition { location, .. } => Some((decode_path(location)?, ChangeKind::Added)),
        Deletion { location, .. } => Some((decode_path(location)?, ChangeKind::Deleted)),
        Modification { location, .. } => Some((decode_path(location)?, ChangeKind::Modified)),
        Rewrite { location, .. } => Some((decode_path(location)?, ChangeKind::Renamed)),
    }
}

fn decode_path(bstr: &gix::bstr::BStr) -> Option<String> {
    std::str::from_utf8(bstr).ok().map(|s| s.to_string())
}

fn classify_status_item(item: &gix::status::Item, out: &mut WorkingTreeStatus) {
    use gix::status::Item;
    match item {
        Item::IndexWorktree(iw) => classify_index_worktree(iw, out),
        Item::TreeIndex(ti) => classify_tree_index(ti, out),
    }
}

fn classify_index_worktree(item: &gix::status::index_worktree::Item, out: &mut WorkingTreeStatus) {
    use gix::status::index_worktree::Item as I;
    match item {
        I::Modification { rela_path, .. } => {
            if let Ok(p) = std::str::from_utf8(rela_path) {
                out.modified.push(p.to_string());
            }
        }
        I::DirectoryContents { entry, .. } => {
            if let Ok(p) = std::str::from_utf8(&entry.rela_path) {
                out.untracked.push(p.to_string());
            }
        }
        I::Rewrite {
            source,
            dirwalk_entry,
            ..
        } => {
            if let Ok(p) = std::str::from_utf8(&dirwalk_entry.rela_path) {
                out.modified.push(p.to_string());
            }
            let _ = source;
        }
    }
}

fn classify_tree_index(change: &gix::diff::index::Change, out: &mut WorkingTreeStatus) {
    use gix::diff::index::Change as C;
    match change {
        C::Addition { location, .. } => {
            if let Some(p) = decode_path(location) {
                out.staged_added.push(p);
            }
        }
        C::Deletion { location, .. } => {
            if let Some(p) = decode_path(location) {
                out.staged_deleted.push(p);
            }
        }
        C::Modification { location, .. } => {
            if let Some(p) = decode_path(location) {
                out.staged_modified.push(p);
            }
        }
        C::Rewrite { location, .. } => {
            if let Some(p) = decode_path(location) {
                out.staged_modified.push(p);
            }
        }
    }
}
