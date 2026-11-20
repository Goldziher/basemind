//! Commit / tree / line-diff helpers extracted from `git/mod.rs` (keeps both files under the
//! 1000-line cap). Everything here still lives behind the `git` module's gix boundary — callers
//! outside `git` see only the typed `Repo` methods and plain structs.
//!
//! Also hosts the typed commit-walk surface the git-history index builds on (`all_commit_shas`,
//! `new_commit_shas`, `commit_record`, `is_ancestor`, `has_commit`) so that `git_history` never
//! touches gix directly.

use super::*;

#[cfg(test)]
thread_local! {
    /// Commits whose changed files [`build_commit_info`] listed on this thread, so a test can tell a
    /// walk that lists files only where it must from one that lists them for every commit it passes.
    pub(crate) static FILE_LISTINGS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(super) fn build_commit_info(local: &gix::Repository, id: gix::ObjectId, include_files: bool) -> Option<CommitInfo> {
    #[cfg(test)]
    if include_files {
        FILE_LISTINGS.with(|listed| listed.set(listed.get() + 1));
    }
    let commit = local.find_commit(id).ok()?;
    let sha = id.to_string();
    let short_sha = sha[..7.min(sha.len())].to_string();
    let message = commit.message().ok();
    let summary = message.as_ref().map(|m| m.summary().to_string()).unwrap_or_default();
    let body = message
        .as_ref()
        .and_then(|m| m.body())
        .map(|b| b.to_string())
        .unwrap_or_default();
    let author_ref = commit.author().ok()?;
    let author = std::str::from_utf8(author_ref.name).unwrap_or("?").to_string();
    let author_email = std::str::from_utf8(author_ref.email).unwrap_or("").to_string();
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
        author_email,
        author_time_unix,
        body,
        files,
    })
}

/// Whether [`build_commit_info`] would list `id`, without building the record: the commit reads and
/// has an author.
fn listable(local: &gix::Repository, id: gix::ObjectId) -> bool {
    local.find_commit(id).is_ok_and(|commit| commit.author().is_ok())
}

/// Diff `commit_id`'s tree against its parent(s) and return (path, change-kind) pairs.
///
/// A merge (including an octopus merge) lists only the files it changed against **every** parent,
/// as `git log --cc` does: a file the merge took unchanged from one branch was changed by that
/// branch's own commits, which the walk lists. Unlike `git log -- <path>`, the walk prunes no side
/// of a merge, so a commit on the side a merge did not keep is still listed. When a file is reported with
/// different statuses against different parents, the higher-severity kind wins (Added > Modified ≈
/// Renamed > Deleted).
pub(super) fn commit_files(
    local: &gix::Repository,
    commit_id: gix::ObjectId,
) -> Option<Vec<(crate::path::RelPath, ChangeKind)>> {
    let commit = local.find_commit(commit_id).ok()?;
    let tree = commit.tree().ok()?;
    let parents: Vec<gix::ObjectId> = commit.parent_ids().map(|p| p.detach()).collect();
    if parents.is_empty() {
        let mut recorder = tree.traverse().breadthfirst.files().ok()?;
        recorder.sort_by(|a, b| a.filepath.cmp(&b.filepath));
        let mut out = Vec::with_capacity(recorder.len());
        // The traversal records every directory on the way to a file; a directory is not a file
        // (as in `classify_tree_change`).
        for e in recorder.into_iter().filter(|e| !e.mode.is_tree()) {
            out.push((crate::path::RelPath::from(e.filepath.as_slice()), ChangeKind::Added));
        }
        return Some(out);
    }

    // Per path: the strongest kind seen so far, how many parents it differs from, and the last
    // parent (by its 1-based count) it was seen against.
    let mut changed: ahash::AHashMap<crate::path::RelPath, (ChangeKind, usize, usize)> = ahash::AHashMap::new();
    let mut compared = 0usize;
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
        compared += 1;
        let _ = platform.for_each_to_obtain_tree(&tree, |change| {
            let mut record = |path, kind| {
                let entry = changed.entry(path).or_insert((kind, 0, 0));
                if change_severity(kind) > change_severity(entry.0) {
                    entry.0 = kind;
                }
                // A path named twice against one parent (a move's source and a new file there)
                // still differs from that parent once.
                if entry.2 != compared {
                    entry.1 += 1;
                    entry.2 = compared;
                }
            };
            if let Some((path, kind)) = classify_tree_change(&change) {
                record(path, kind);
            }
            // A move names its destination as `renamed`; its source is gone from this commit's tree,
            // so it is listed as deleted too. Without it the history of the source's directory
            // missed the commit that moved the last file out of it.
            if let gix::object::tree::diff::Change::Rewrite {
                source_location,
                source_entry_mode,
                copy: false,
                ..
            } = &change
                && !source_entry_mode.is_tree()
                && let Some(source) = decode_path(source_location)
            {
                record(source, ChangeKind::Deleted);
            }
            Ok(gix::object::tree::diff::Action::Continue(()))
        });
    }
    let mut out: Vec<(crate::path::RelPath, ChangeKind)> = changed
        .into_iter()
        .filter(|(_, (_, parents, _))| *parents == compared)
        .map(|(path, (kind, _, _))| (path, kind))
        .collect();
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

/// Whether `commit_id` touched the file at the exact path `rel`. Exact-path match — it does
/// **not** follow renames, so history built on it (`log_for_path`, `commits_touching`,
/// `symbol_history`) stops at a rename. Deliberate asymmetry with [`Repo::blame_file`], which
/// passes `Rewrites::default()` to gix and follows renames line-by-line.
pub(super) fn commit_touches_path(local: &gix::Repository, commit_id: gix::ObjectId, rel: &[u8]) -> bool {
    let components: Vec<&[u8]> = rel.split(|&b| b == b'/').filter(|c| !c.is_empty()).collect();
    if components.is_empty() {
        return false;
    }
    let Ok(commit) = local.find_commit(commit_id) else {
        return false;
    };
    let Ok(tree) = commit.tree() else {
        return false;
    };
    let current = entry_ident_at(&tree, &components);

    let parents: Vec<gix::ObjectId> = commit.parent_ids().map(|p| p.detach()).collect();
    if parents.is_empty() {
        return current.is_some();
    }
    // A merge touches the path only when it differs there from every parent: one that took the path
    // unchanged from a branch did not change it. No side is pruned, unlike `git log -- <path>`.
    parents.into_iter().all(|pid| {
        let parent = local
            .find_commit(pid)
            .ok()
            .and_then(|pc| pc.tree().ok())
            .and_then(|pt| entry_ident_at(&pt, &components));
        current != parent
    })
}

/// `(blob oid, mode)` of the entry at the given path components in `tree`, or `None` when the
/// path is absent. `lookup_entry` follows the path component-by-component without recursing into
/// sibling subtrees, so this is O(path depth) tree reads — not a full tree diff. The mode is part
/// of the identity so a pure mode flip still registers as a change, matching a real diff.
fn entry_ident_at(tree: &gix::Tree<'_>, components: &[&[u8]]) -> Option<(gix::ObjectId, gix::object::tree::EntryMode)> {
    let entry = tree.lookup_entry(components.iter().copied()).ok().flatten()?;
    Some((entry.object_id(), entry.mode()))
}

/// Line-diff between two byte buffers using the histogram algorithm + slider heuristics.
/// Returns one `Hunk` per `imara_diff::Hunk` — runs without surrounding context lines,
/// because agents have direct access to the source via the outline tools — and whether a hunk line
/// was not valid UTF-8. Such a line is kept, its invalid bytes decoded as U+FFFD, so a hunk's text
/// always holds as many lines as its counts say.
pub(super) fn compute_hunks(old: &[u8], new: &[u8]) -> (Vec<Hunk>, bool) {
    use gix::diff::blob::{Algorithm, InternedInput, diff_with_slider_heuristics, sources};

    let input = InternedInput::new(sources::byte_lines(old), sources::byte_lines(new));
    let diff = diff_with_slider_heuristics(Algorithm::Histogram, &input);

    let old_lines = line_byte_offsets(old);
    let new_lines = line_byte_offsets(new);
    let mut out = Vec::with_capacity(diff.count_additions() as usize + diff.count_removals() as usize);
    let mut lossy = false;
    for hunk in diff.hunks() {
        let removed_count = hunk.before.end - hunk.before.start;
        let added_count = hunk.after.end - hunk.after.start;
        let kind = match (removed_count, added_count) {
            (0, _) => HunkKind::Added,
            (_, 0) => HunkKind::Removed,
            _ => HunkKind::Modified,
        };
        let mut removed = String::new();
        lossy |= push_lines(&mut removed, old, &old_lines, hunk.before.start..hunk.before.end);
        let mut added = String::new();
        lossy |= push_lines(&mut added, new, &new_lines, hunk.after.start..hunk.after.end);
        let text = if removed_count == 0 {
            added
        } else if added_count == 0 {
            removed
        } else {
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
    (out, lossy)
}

/// Append the lines `lines` (0-based) of `buf` to `out`, each decoded lossily; true when one of
/// them was not valid UTF-8.
fn push_lines(out: &mut String, buf: &[u8], offsets: &[(u32, u32)], lines: std::ops::Range<u32>) -> bool {
    let mut lossy = false;
    for line in lines {
        if let Some(&(start, end)) = offsets.get(line as usize) {
            let text = String::from_utf8_lossy(&buf[start as usize..end as usize]);
            lossy |= matches!(text, std::borrow::Cow::Owned(_));
            out.push_str(&text);
        }
    }
    lossy
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

/// The file a tree change names, and how it changed. A directory is not a file: the tree diff
/// reports every directory on the way to a changed file as modified, and those entries are left
/// out. An entry that turned from a file into a directory is the file's deletion, and the reverse
/// its addition.
pub(super) fn classify_tree_change(
    change: &gix::object::tree::diff::Change<'_, '_, '_>,
) -> Option<(crate::path::RelPath, ChangeKind)> {
    use gix::object::tree::diff::Change::*;
    let kind = match change {
        Addition { entry_mode, .. } | Deletion { entry_mode, .. } | Rewrite { entry_mode, .. }
            if entry_mode.is_tree() =>
        {
            return None;
        }
        Addition { .. } => ChangeKind::Added,
        Deletion { .. } => ChangeKind::Deleted,
        Rewrite { .. } => ChangeKind::Renamed,
        Modification {
            previous_entry_mode,
            entry_mode,
            ..
        } => match (previous_entry_mode.is_tree(), entry_mode.is_tree()) {
            (false, false) => ChangeKind::Modified,
            (true, false) => ChangeKind::Added,
            (false, true) => ChangeKind::Deleted,
            (true, true) => return None,
        },
    };
    Some((decode_path(change.location())?, kind))
}

pub(super) fn decode_path(bstr: &gix::bstr::BStr) -> Option<crate::path::RelPath> {
    Some(crate::path::RelPath::from(<gix::bstr::BStr as AsRef<[u8]>>::as_ref(
        bstr,
    )))
}

/// One step of a history walk, as a result. A commit the walk cannot read (missing or corrupt) is an
/// error: skipping it would end the history there, its ancestors never queued, with nothing to say
/// the list is short. A shallow clone's boundary is not such a step, since gix's walk honours
/// `.git/shallow` and never asks for the parents the clone lacks.
pub(super) fn walk_step<T, E: std::fmt::Display>(step: Result<T, E>) -> Result<T, GitError> {
    step.map_err(|error| GitError::Read {
        what: "rev walk".to_string(),
        msg: error.to_string(),
    })
}

impl Repo {
    /// Resolve a rev-spec (sha / branch / HEAD) to a detached gix [`ObjectId`].
    fn resolve_oid(local: &gix::Repository, rev: &str) -> Option<gix::ObjectId> {
        local.rev_parse_single(rev).ok().map(|id| id.detach())
    }

    /// All commit shas reachable from HEAD, **newest-first** (40-char hex). Empty when HEAD is
    /// unborn. Used for the git-history full rebuild — the caller assigns dense ordinals.
    pub fn all_commit_shas(&self) -> Result<Vec<String>, GitError> {
        let local = self.local();
        let head = match local.head_id() {
            Ok(h) => h.detach(),
            Err(_) => return Ok(Vec::new()),
        };
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
        walk.map(|step| walk_step(step).map(|info| info.id.to_string()))
            .collect()
    }

    /// The commits at `skip..skip + take` of [`Repo::log_paths`]' list for `head`, with their
    /// changed files when `include_files`. The `skip` commits before them are walked without listing
    /// files, so a page deep in the history costs one walk to it and a file listing per commit on the
    /// page.
    pub fn log_window(
        &self,
        head: &str,
        skip: usize,
        take: usize,
        include_files: bool,
    ) -> Result<Vec<CommitInfo>, GitError> {
        self.window(head, skip, take, include_files, |_, _| true)
    }

    /// The commits at `skip..skip + take` of [`Repo::log_for_path`]'s list for `head`: those that
    /// changed the entry at exactly `rel`. The commits before them are only tested against `rel`.
    pub fn log_window_touching(
        &self,
        head: &str,
        rel: &crate::path::RelPath,
        skip: usize,
        take: usize,
    ) -> Result<Vec<CommitInfo>, GitError> {
        self.window(head, skip, take, false, |local, id| {
            commit_touches_path(local, id, rel.as_bytes())
        })
    }

    /// The commits at `skip..skip + take` of `head`'s newest-first history that `keep` admits.
    fn window(
        &self,
        head: &str,
        skip: usize,
        take: usize,
        include_files: bool,
        keep: impl Fn(&gix::Repository, gix::ObjectId) -> bool,
    ) -> Result<Vec<CommitInfo>, GitError> {
        let local = self.local();
        let Some(head) = Self::resolve_oid(&local, head) else {
            return Err(GitError::RevParse {
                rev: head.to_string(),
                msg: "does not resolve to a commit".to_string(),
            });
        };
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
        let mut kept = 0usize;
        let mut out = Vec::new();
        for step in walk {
            if out.len() >= take {
                break;
            }
            let id = walk_step(step)?.id;
            // Offsets count the commits the whole-list walks list: one that does not decode is
            // skipped there, so it is not counted here either.
            if !keep(&local, id) || !listable(&local, id) {
                continue;
            }
            kept += 1;
            if kept > skip
                && let Some(commit) = build_commit_info(&local, id, include_files)
            {
                out.push(commit);
            }
        }
        Ok(out)
    }

    /// Commit shas reachable from HEAD but **not** from `hidden` (the previously-indexed head),
    /// newest-first. The incremental-append set. `hidden` that no longer resolves yields all
    /// commits (caller should have detected the rewrite via [`Repo::is_ancestor`] first).
    pub fn new_commit_shas(&self, hidden: &str) -> Result<Vec<String>, GitError> {
        let local = self.local();
        let head = match local.head_id() {
            Ok(h) => h.detach(),
            Err(_) => return Ok(Vec::new()),
        };
        let mut walk = local
            .rev_walk([head])
            .sorting(gix::revision::walk::Sorting::ByCommitTime(
                gix::traverse::commit::simple::CommitTimeOrder::NewestFirst,
            ));
        if let Some(hidden_id) = Self::resolve_oid(&local, hidden) {
            walk = walk.with_hidden([hidden_id]);
        }
        let walk = walk.all().map_err(|e| GitError::Read {
            what: "rev walk (hidden)".to_string(),
            msg: e.to_string(),
        })?;
        walk.map(|step| walk_step(step).map(|info| info.id.to_string()))
            .collect()
    }

    /// Full record (header + per-file change list, union-across-parents) for one commit sha — the
    /// exact relation the live history tools filter on, so the index inherits output parity.
    pub fn commit_record(&self, sha: &str) -> Option<CommitInfo> {
        let local = self.local();
        let id = Self::resolve_oid(&local, sha)?;
        build_commit_info(&local, id, true)
    }

    /// True when `ancestor` is an ancestor of (or equal to) `descendant`. Uses the commit-graph-
    /// backed merge-base: `a` is an ancestor of `b` iff `merge_base(a, b) == a`.
    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> bool {
        let local = self.local();
        let (Some(and), Some(desc)) = (
            Self::resolve_oid(&local, ancestor),
            Self::resolve_oid(&local, descendant),
        ) else {
            return false;
        };
        if and == desc {
            return true;
        }
        local
            .merge_base(and, desc)
            .ok()
            .flatten()
            .is_some_and(|base| base.detach() == and)
    }

    /// True when the object named by `sha` exists in the repository.
    pub fn has_commit(&self, sha: &str) -> bool {
        let local = self.local();
        Self::resolve_oid(&local, sha).is_some()
    }
}
