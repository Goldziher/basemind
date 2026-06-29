//! Commit / tree / line-diff helpers extracted from `git/mod.rs` (keeps both files under the
//! 1000-line cap). Everything here still lives behind the `git` module's gix boundary — callers
//! outside `git` see only the typed `Repo` methods and plain structs.
//!
//! Also hosts the typed commit-walk surface the git-history index builds on (`all_commit_shas`,
//! `new_commit_shas`, `commit_record`, `is_ancestor`, `has_commit`) so that `git_history` never
//! touches gix directly.

use super::*;

pub(super) fn build_commit_info(
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
pub(super) fn commit_files(
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

/// Whether `commit_id` touched the file at the exact path `rel`. Exact-path match — it does
/// **not** follow renames, so history built on it (`log_for_path`, `commits_touching`,
/// `symbol_history`) stops at a rename. Deliberate asymmetry with [`Repo::blame_file`], which
/// passes `Rewrites::default()` to gix and follows renames line-by-line.
pub(super) fn commit_touches_path(
    local: &gix::Repository,
    commit_id: gix::ObjectId,
    rel: &[u8],
) -> bool {
    // Path-scoped TREESAME check: compare the entry at `rel` in this commit's tree against
    // each parent's, by (blob oid, mode). `lookup_entry` walks the path component-by-component
    // and never recurses into sibling subtrees, so this is O(path depth) tree object reads per
    // commit instead of the full recursive tree diff `commit_files` performs. That difference is
    // what keeps `log_for_path` / `commits_touching` sub-second on a deep monorepo (200k+
    // commits) — a full diff per walked commit pushes a single query into minutes.
    //
    // Semantics match `commit_files`' union-across-parents: the path is "touched" when it
    // differs from at least one parent (covers add / modify / delete / mode-change). Exact-path
    // only — like the diff path, it does not follow renames.
    let components: Vec<&[u8]> = rel
        .split(|&b| b == b'/')
        .filter(|c| !c.is_empty())
        .collect();
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
        // Root commit — every entry is "added", so the path is touched iff it exists.
        return current.is_some();
    }
    parents.into_iter().any(|pid| {
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
fn entry_ident_at(
    tree: &gix::Tree<'_>,
    components: &[&[u8]],
) -> Option<(gix::ObjectId, gix::object::tree::EntryMode)> {
    let entry = tree
        .lookup_entry(components.iter().copied())
        .ok()
        .flatten()?;
    Some((entry.object_id(), entry.mode()))
}

/// Line-diff between two byte buffers using the histogram algorithm + slider heuristics.
/// Returns one `Hunk` per `imara_diff::Hunk` — runs without surrounding context lines,
/// because agents have direct access to the source via the outline tools.
pub(super) fn compute_hunks(old: &[u8], new: &[u8]) -> Vec<Hunk> {
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

pub(super) fn decode_path(bstr: &gix::bstr::BStr) -> Option<crate::path::RelPath> {
    // Preserve raw bytes — gix hands us paths in the on-disk byte form. The discriminated
    // serde wire format on RelPath round-trips non-UTF-8 bytes losslessly to MCP clients.
    Some(crate::path::RelPath::from(<gix::bstr::BStr as AsRef<
        [u8],
    >>::as_ref(bstr)))
}

// ── typed commit-walk surface for the git-history index ──────────────────────

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
            Err(_) => return Ok(Vec::new()), // unborn HEAD
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
        Ok(walk
            .filter_map(|i| i.ok())
            .map(|i| i.id.to_string())
            .collect())
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
        Ok(walk
            .filter_map(|i| i.ok())
            .map(|i| i.id.to_string())
            .collect())
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
            .map(|base| base.detach() == and)
            .unwrap_or(false)
    }

    /// True when the object named by `sha` exists in the repository.
    pub fn has_commit(&self, sha: &str) -> bool {
        let local = self.local();
        Self::resolve_oid(&local, sha).is_some()
    }
}
