//! Thin gitoxide wrapper.
//!
//! Everything that touches `gix` lives in this module. The rest of the crate sees plain
//! `String` / `PathBuf` / small structs so we can swap the underlying git library later
//! without rewriting half of the codebase.

mod remote;
pub use remote::{normalize_remote_url, scope_key};

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
    /// Blame refused: the file exceeds the configured byte/line cap. Tunable via
    /// `BASEMIND_BLAME_MAX_BYTES` (default 1 MiB) and `BASEMIND_BLAME_MAX_LINES` (default 5 000).
    #[error("blame skipped: {path} is too large ({bytes} bytes, {lines} lines)")]
    BlameTooLarge {
        path: crate::path::RelPath,
        bytes: u64,
        lines: u64,
    },
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
    /// True if `.git/shallow` exists — history walks may terminate at the boundary, and
    /// blame/diff_outline can hit "tree iterator" errors near the cut. Surfaced through
    /// `Repo::is_shallow()` so MCP tools can flag responses as truncated rather than fail.
    is_shallow: bool,
}

#[derive(Debug, Default, Clone)]
pub struct WorkingTreeStatus {
    pub staged_added: Vec<crate::path::RelPath>,
    pub staged_modified: Vec<crate::path::RelPath>,
    pub staged_deleted: Vec<crate::path::RelPath>,
    pub modified: Vec<crate::path::RelPath>,
    pub untracked: Vec<crate::path::RelPath>,
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
    pub files: Vec<(crate::path::RelPath, ChangeKind)>,
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
    pub source_path: Option<crate::path::RelPath>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BlameResult {
    pub path: crate::path::RelPath,
    pub suspect_sha: String,
    pub hunks: Vec<BlameHunk>,
    /// Set when blame was cut short — currently only fires for shallow clones where gix
    /// hits its "could not find existing iterator over a tree" error walking past the
    /// shallow boundary. Hunks contain whatever was resolvable before the cut. `String`
    /// (not `&'static str`) because this struct is round-tripped through the on-disk
    /// blame cache via serde.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<String>,
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
        let is_shallow = inner.path().join("shallow").exists();
        Ok(Self {
            inner,
            workdir,
            is_shallow,
        })
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// True when the underlying clone is shallow (`.git/shallow` exists). History walks may
    /// terminate at the shallow boundary; MCP tools mark responses as `truncated` instead
    /// of erroring.
    pub fn is_shallow(&self) -> bool {
        self.is_shallow
    }

    /// Borrow a per-thread `Repository` view of the wrapped thread-safe repo.
    pub(super) fn local(&self) -> gix::Repository {
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

    /// Return the worktree-relative roots of every submodule declared in `.gitmodules`.
    /// Paths use forward slashes; the list is empty when `.gitmodules` is absent, malformed,
    /// or has no entries. Errors from gix are downgraded to an empty list — submodule
    /// awareness is a hint to the scanner, not a hard requirement.
    pub fn submodule_paths(&self) -> Vec<crate::path::RelPath> {
        let local = self.local();
        let iter = match local.submodules() {
            Ok(Some(it)) => it,
            _ => return Vec::new(),
        };
        let mut out = Vec::new();
        for sm in iter {
            let path = match sm.path() {
                Ok(cow) => {
                    let bstr: &gix::bstr::BStr = &cow;
                    crate::path::RelPath::from(<gix::bstr::BStr as AsRef<[u8]>>::as_ref(bstr))
                }
                Err(_) => continue,
            };
            if !path.is_empty() {
                out.push(path);
            }
        }
        out.sort();
        out.dedup();
        out
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
    pub fn read_blob_at_rev(
        &self,
        rev_sha: &str,
        rel: impl AsRef<Path>,
    ) -> Result<Option<Vec<u8>>, GitError> {
        Ok(self
            .read_blob_at_rev_with_oid(rev_sha, rel)?
            .map(|(bytes, _)| bytes))
    }

    /// Like `read_blob_at_rev` but also returns the blob's git OID. Callers caching parsed
    /// outlines key on `(oid, lang)` to skip re-parsing identical blobs across commits.
    pub fn read_blob_at_rev_with_oid(
        &self,
        rev_sha: &str,
        rel: impl AsRef<Path>,
    ) -> Result<Option<(Vec<u8>, gix::ObjectId)>, GitError> {
        let local = self.local();
        let tree = resolve_tree(&local, rev_sha)?;
        let rel_ref = rel.as_ref();
        let rel_display = rel_ref.display();
        let entry = match tree
            .lookup_entry_by_path(rel_ref)
            .map_err(|e| GitError::Read {
                what: format!("{rev_sha}:{rel_display}"),
                msg: e.to_string(),
            })? {
            Some(e) => e,
            None => return Ok(None),
        };
        let oid = entry.object_id();
        let object = entry.object().map_err(|e| GitError::Read {
            what: format!("{rev_sha}:{rel_display}"),
            msg: e.to_string(),
        })?;
        let blob = object.try_into_blob().map_err(|e| GitError::Read {
            what: format!("{rev_sha}:{rel_display}"),
            msg: format!("not a blob: {e}"),
        })?;
        Ok(Some((blob.data.clone(), oid)))
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
    pub fn log_for_path(
        &self,
        rel: impl AsRef<Path>,
        max_commits: usize,
    ) -> Result<Vec<CommitInfo>, GitError> {
        let rel_bytes_buf: bstr::BString =
            bstr::BString::from(rel.as_ref().as_os_str().as_encoded_bytes());
        let rel: &[u8] = rel_bytes_buf.as_slice();
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
        path: &crate::path::RelPath,
        line_range: Option<(u32, u32)>,
    ) -> Result<BlameResult, GitError> {
        use gix::bstr::BStr;

        let local = self.local();

        // Size cap: blame's per-line work is O(history); a 100k-line lockfile blame swamps
        // the worker. Reject up front with a typed error the caller can surface cleanly.
        // We peek at the suspect-rev blob (not the working tree) because that's what gets
        // blamed; for HEAD this is what's on disk anyway.
        let (size_bytes, line_count) =
            blob_size_and_line_count(&local, suspect_sha, path).unwrap_or((0, 0));
        let max_bytes = blame_max_bytes_from_env();
        let max_lines = blame_max_lines_from_env();
        if size_bytes > max_bytes || line_count > max_lines {
            return Err(GitError::BlameTooLarge {
                path: path.clone(),
                bytes: size_bytes,
                lines: line_count,
            });
        }

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

        let outcome = match local.blame_file(BStr::new(path.as_bytes()), suspect, options) {
            Ok(o) => o,
            Err(e) => {
                // Shallow clones routinely trip gix's history walk near the cut. Surface a
                // graceful truncated result rather than failing the whole call — the caller
                // (MCP) reports `truncated_reason` so the agent knows what happened.
                if self.is_shallow && looks_like_shallow_blame_error(&e) {
                    return Ok(BlameResult {
                        path: path.clone(),
                        suspect_sha: suspect_sha.to_string(),
                        hunks: Vec::new(),
                        truncated_reason: Some("shallow_clone".to_string()),
                    });
                }
                return Err(GitError::Read {
                    what: format!("blame {suspect_sha}:{path}"),
                    msg: e.to_string(),
                });
            }
        };

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
                .map(|b| crate::path::RelPath::from(b.as_slice()));
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
            path: path.clone(),
            suspect_sha: suspect_sha.to_string(),
            hunks,
            truncated_reason: None,
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
        path: impl AsRef<Path>,
    ) -> Result<Option<(Vec<Hunk>, bool, bool)>, GitError> {
        let path_ref = path.as_ref();
        let old_bytes = self.read_blob_at_rev(rev_old, path_ref)?;
        let new_bytes = self.read_blob_at_rev(rev_new, path_ref)?;
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
    ) -> Result<Vec<(crate::path::RelPath, ChangeKind)>, GitError> {
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

/// Default blame size cap in bytes (1 MiB) — protects against vendored bundles + lockfiles.
const BLAME_DEFAULT_MAX_BYTES: u64 = 1 << 20;
/// Default blame line cap (5 000) — protects against generated single-line monsters that
/// pass the byte cap.
const BLAME_DEFAULT_MAX_LINES: u64 = 5_000;

fn blame_max_bytes_from_env() -> u64 {
    std::env::var("BASEMIND_BLAME_MAX_BYTES")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(BLAME_DEFAULT_MAX_BYTES)
}

fn blame_max_lines_from_env() -> u64 {
    std::env::var("BASEMIND_BLAME_MAX_LINES")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(BLAME_DEFAULT_MAX_LINES)
}

/// Read the suspect-rev blob and compute `(bytes, lines)` for the size-cap pre-check.
/// Failure is non-fatal — we return `None` and let the blame call fail with its own error.
fn blob_size_and_line_count(
    local: &gix::Repository,
    rev: &str,
    path: &crate::path::RelPath,
) -> Option<(u64, u64)> {
    let id = local.rev_parse_single(rev).ok()?.detach();
    let tree = local.find_commit(id).ok()?.tree().ok()?;
    let entry = tree.lookup_entry_by_path(path).ok()??;
    let obj = entry.object().ok()?;
    if obj.kind != gix::object::Kind::Blob {
        return None;
    }
    let data = &obj.data;
    let bytes = data.len() as u64;
    // memchr is already a dep; line count = (NL count) + (1 if no trailing NL && non-empty).
    let nls = memchr::memchr_iter(b'\n', data).count() as u64;
    let lines = if bytes == 0 {
        0
    } else if data.last() == Some(&b'\n') {
        nls
    } else {
        nls + 1
    };
    Some((bytes, lines))
}

/// Heuristic match for gix's shallow-related blame failures. The exact wording lives in
/// gix internals and could change between versions; we match on the symptom phrase rather
/// than a typed error to stay resilient across point releases.
fn looks_like_shallow_blame_error<E: std::fmt::Display>(err: &E) -> bool {
    let msg = err.to_string();
    msg.contains("Could not find existing iterator over a tree")
        || msg.contains("Could not find commit")
        || msg.contains("shallow")
}

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

/// Diff `commit_id`'s tree against its parent(s) and return (path, change-kind) pairs.
///
/// For commits with multiple parents (merges, including octopus merges with ≥3 parents) we
/// **union** the diffs against every parent so files changed on a non-first branch leg
/// still surface — `git log -m`-style semantics rather than first-parent default. When a
/// file is reported with different statuses against different parents, the higher-severity
/// kind wins (Added > Modified ≈ Renamed > Deleted).
fn commit_files(
    local: &gix::Repository,
    commit_id: gix::ObjectId,
) -> Option<Vec<(crate::path::RelPath, ChangeKind)>> {
    let commit = local.find_commit(commit_id).ok()?;
    let tree = commit.tree().ok()?;
    let parents: Vec<gix::ObjectId> = commit.parent_ids().map(|p| p.detach()).collect();
    if parents.is_empty() {
        // Initial commit — every entry is "added".
        let mut recorder = tree.traverse().breadthfirst.files().ok()?;
        recorder.sort_by(|a, b| a.filepath.cmp(&b.filepath));
        let mut out = Vec::with_capacity(recorder.len());
        for e in recorder {
            out.push((
                crate::path::RelPath::from(e.filepath.as_slice()),
                ChangeKind::Added,
            ));
        }
        return Some(out);
    }

    // path → strongest ChangeKind seen across all parent diffs.
    let mut union: ahash::AHashMap<crate::path::RelPath, ChangeKind> = ahash::AHashMap::new();
    for pid in parents {
        let Ok(parent_commit) = local.find_commit(pid) else {
            continue;
        };
        let Ok(parent_tree) = parent_commit.tree() else {
            continue;
        };
        let mut platform = match parent_tree.changes() {
            Ok(p) => p,
            Err(_) => continue,
        };
        // `for_each_to_obtain_tree(other, ..)` walks the changes needed to convert `self` →
        // `other`. From parent → commit gives us commit-relative-to-this-parent.
        let _ = platform.for_each_to_obtain_tree(&tree, |change| {
            if let Some((path, kind)) = classify_tree_change(&change) {
                union
                    .entry(path)
                    .and_modify(|existing| {
                        if change_severity(kind) > change_severity(*existing) {
                            *existing = kind;
                        }
                    })
                    .or_insert(kind);
            }
            Ok::<_, std::convert::Infallible>(gix::object::tree::diff::Action::Continue(()))
        });
    }
    let mut out: Vec<(crate::path::RelPath, ChangeKind)> = union.into_iter().collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Some(out)
}

/// Priority ordering used when the same file shows up with different kinds across multiple
/// parent diffs. Higher number = takes precedence. Added beats Modified because the file is
/// genuinely new from at least one merge leg's perspective.
fn change_severity(k: ChangeKind) -> u8 {
    match k {
        ChangeKind::Added => 3,
        ChangeKind::Renamed | ChangeKind::Modified => 2,
        ChangeKind::Deleted => 1,
    }
}

fn commit_touches_path(local: &gix::Repository, commit_id: gix::ObjectId, rel: &[u8]) -> bool {
    let Some(files) = commit_files(local, commit_id) else {
        return false;
    };
    files.iter().any(|(p, _)| p.as_bytes() == rel)
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
) -> Option<(crate::path::RelPath, ChangeKind)> {
    use gix::object::tree::diff::Change::*;
    match change {
        Addition { location, .. } => Some((decode_path(location)?, ChangeKind::Added)),
        Deletion { location, .. } => Some((decode_path(location)?, ChangeKind::Deleted)),
        Modification { location, .. } => Some((decode_path(location)?, ChangeKind::Modified)),
        Rewrite { location, .. } => Some((decode_path(location)?, ChangeKind::Renamed)),
    }
}

fn decode_path(bstr: &gix::bstr::BStr) -> Option<crate::path::RelPath> {
    // Preserve raw bytes — gix hands us paths in the on-disk byte form. The discriminated
    // serde wire format on RelPath round-trips non-UTF-8 bytes losslessly to MCP clients.
    Some(crate::path::RelPath::from(<gix::bstr::BStr as AsRef<
        [u8],
    >>::as_ref(bstr)))
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
            out.modified
                .push(crate::path::RelPath::from(rela_path.as_slice()));
        }
        I::DirectoryContents { entry, .. } => {
            out.untracked
                .push(crate::path::RelPath::from(entry.rela_path.as_slice()));
        }
        I::Rewrite {
            source,
            dirwalk_entry,
            ..
        } => {
            out.modified.push(crate::path::RelPath::from(
                dirwalk_entry.rela_path.as_slice(),
            ));
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
