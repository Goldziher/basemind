//! Change scope: the set of files a change touches, which narrows a query to them (`changed` on
//! `code_grep`, `code_symbols`, `code_refs`, `find_files` and `code_search`; the diff target of
//! `impact`; the briefing call).
//!
//! The contract later callers rely on:
//!
//! - A change is named by a [`ChangeSpec`]: a revision, whose merge base with `HEAD` is diffed
//!   against the working tree (`""` or `HEAD` is the working tree against `HEAD`); `A..B` diffed
//!   tree to tree; `A...B`, the merge base of `A` and `B` diffed tree to tree against `B`, which is
//!   what `B`'s side changed since the two diverged (in both forms an empty side is `HEAD`, as in
//!   git); or the staged changes. A `+importers` suffix asks for the files that import the changed
//!   ones; this module only parses it, and the caller expands the scope through the file-edge graph.
//! - A working-tree scope of `<rev>` holds what `git diff --name-only $(git merge-base <rev> HEAD)`
//!   lists plus every untracked file, listed one by one however `status.showUntrackedFiles`
//!   collapses them: a branch's own changes, committed or not, without what `<rev>` gained since the
//!   branch left it. A file changed since the merge base and then restored on disk to its content
//!   there is left out; one the comparison cannot read as a regular file (a symlink, a submodule) is
//!   kept, so a scope never misses a change it could not check.
//! - Paths are repository-relative, as the index keys them. Deleted files and both sides of a
//!   rename are in the scope, so the caller filters to what the index holds.
//! - [`ChangeScope::len`] is the `scope_files` count. An empty scope is an answer, not an error:
//!   the caller reports `scope_files: 0`, which tells "nothing changed" from "no hit in the change".
//! - An unknown revision is [`EngineError::InvalidArgument`] naming it and the accepted forms, and so
//!   are two revisions whose histories share no commit, which have no merge base.
//!
//! Everything goes through `crate::git` (gix); nothing shells out.

#![cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "foundation D: the `changed` argument and the impact report use it"
    )
)]

use ahash::AHashSet;

use super::EngineError;
use crate::git::{GitError, Repo};
use crate::path::RelPath;

/// The suffix on a `changed` value that asks for the changed files' importers as well.
pub(crate) const IMPORTERS_SUFFIX: &str = "+importers";

/// Which change a scope is taken from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChangeBase {
    /// The working tree against a revision; `None` is `HEAD`.
    WorkingTree(Option<String>),
    /// The tree of `from` against the tree of `to`.
    Range { from: String, to: String },
    /// The tree of the merge base of `from` and `to` against the tree of `to`.
    MergeBase { from: String, to: String },
    /// The staging index against `HEAD`.
    Staged,
}

/// A parsed `changed` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChangeSpec {
    pub(crate) base: ChangeBase,
    /// The value ended in [`IMPORTERS_SUFFIX`].
    pub(crate) importers: bool,
}

impl ChangeSpec {
    /// Parses a `changed` value: `<rev>`, `""`, `A..B`, `A..`, `..B`, `A...B`, `A...` or `...B`, each
    /// optionally followed by `+importers`. Surrounding whitespace is ignored. `..` and `...` alone
    /// are refused; whether a revision exists is checked by [`change_scope`], not here. The staged
    /// base has no spelling in a value and is built directly.
    pub(crate) fn parse(value: &str) -> Result<Self, EngineError> {
        let value = value.trim();
        let (value, importers) = match value.strip_suffix(IMPORTERS_SUFFIX) {
            Some(rest) => (rest.trim_end(), true),
            None => (value, false),
        };
        let base = match (value.split_once("..."), value.split_once("..")) {
            (Some(("", "")), _) | (None, Some(("", ""))) => return Err(refuse_form(value, "names no revision")),
            (Some((from, to)), _) => ChangeBase::MergeBase {
                from: or_head(from),
                to: or_head(to),
            },
            (None, Some((from, to))) => ChangeBase::Range {
                from: or_head(from),
                to: or_head(to),
            },
            (None, None) if value.is_empty() || value == "HEAD" => ChangeBase::WorkingTree(None),
            (None, None) => ChangeBase::WorkingTree(Some(value.to_string())),
        };
        Ok(Self { base, importers })
    }
}

fn or_head(side: &str) -> String {
    if side.is_empty() { "HEAD" } else { side }.to_string()
}

fn refuse_form(value: &str, why: &str) -> EngineError {
    EngineError::invalid_argument(format!(
        "change `{value}` {why}; pass a revision (\"\" for HEAD), `A..B` or `A...B`, optionally followed \
         by `{IMPORTERS_SUFFIX}`"
    ))
}

/// The files one change touches.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ChangeScope {
    paths: AHashSet<RelPath>,
}

impl ChangeScope {
    /// Whether `path` is in the scope.
    pub(crate) fn contains(&self, path: &RelPath) -> bool {
        self.paths.contains(path)
    }

    /// Files in the scope: the `scope_files` count.
    pub(crate) fn len(&self) -> usize {
        self.paths.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// The files in the scope, in no order.
    pub(crate) fn paths(&self) -> &AHashSet<RelPath> {
        &self.paths
    }

    /// Adds files to the scope (the importers an expansion found).
    pub(crate) fn extend(&mut self, paths: impl IntoIterator<Item = RelPath>) {
        self.paths.extend(paths);
    }
}

/// The scope of `base` in `repo`. See the module docs for what each base holds.
pub(crate) fn change_scope(repo: &Repo, base: &ChangeBase) -> Result<ChangeScope, EngineError> {
    let paths = match base {
        ChangeBase::Staged => {
            let status = repo.status_per_file()?;
            [status.staged_added, status.staged_modified, status.staged_deleted]
                .into_iter()
                .flatten()
                .collect()
        }
        ChangeBase::Range { from, to } => repo
            .changed_paths_between(from, to)
            .map_err(|error| unknown_revision(error, &format!("{from}..{to}")))?
            .into_iter()
            .collect(),
        ChangeBase::MergeBase { from, to } => {
            let value = format!("{from}...{to}");
            let base = merge_base(repo, from, to, &value)?;
            repo.changed_paths_between(&base, to)
                .map_err(|error| unknown_revision(error, &value))?
                .into_iter()
                .collect()
        }
        ChangeBase::WorkingTree(rev) => working_tree(repo, rev.as_deref())?,
    };
    Ok(ChangeScope { paths })
}

/// The working tree against the merge base of `rev` and `HEAD` (`None`: `HEAD`): the status, which
/// is the working tree against `HEAD`, joined with the merge base's tree against `HEAD`'s. Only a
/// file in both can have returned to its content at the merge base, so only those are compared on
/// disk.
fn working_tree(repo: &Repo, rev: Option<&str>) -> Result<AHashSet<RelPath>, EngineError> {
    let status = repo.status_per_file()?;
    let mut paths: AHashSet<RelPath> = [
        status.staged_added,
        status.staged_modified,
        status.staged_deleted,
        status.modified,
        status.untracked,
    ]
    .into_iter()
    .flatten()
    .collect();
    let Some(rev) = rev else {
        return Ok(paths);
    };
    let commit = merge_base(repo, rev, "HEAD", rev)?;
    let committed = repo
        .changed_paths_between(&commit, "HEAD")
        .map_err(|error| unknown_revision(error, rev))?;
    for path in committed {
        if !paths.contains(&path) {
            paths.insert(path);
        } else if on_disk_as_at(repo, &commit, &path)? {
            paths.remove(&path);
        }
    }
    Ok(paths)
}

/// Whether the working tree holds `path` exactly as commit `commit` does: both absent, or both a
/// regular file with the same bytes. Anything it cannot compare is not the same.
fn on_disk_as_at(repo: &Repo, commit: &str, path: &RelPath) -> Result<bool, EngineError> {
    let Ok(file) = crate::path::lexical_file(repo.workdir(), path) else {
        return Ok(false);
    };
    let on_disk = match std::fs::symlink_metadata(&file) {
        Ok(meta) if meta.is_file() => Some(std::fs::read(&file)?),
        Ok(_) => return Ok(false),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            None
        }
        Err(error) => return Err(error.into()),
    };
    // A submodule or symlink entry at `commit` is not a readable blob: kept as changed, as above.
    let Ok(at_commit) = repo.read_blob_at_rev(commit, path) else {
        return Ok(false);
    };
    Ok(at_commit == on_disk)
}

/// The merge base of `one` and `two`, which `value` (the form the caller passed) names. Histories
/// that share no commit have none, which the caller must fix.
fn merge_base(repo: &Repo, one: &str, two: &str, value: &str) -> Result<String, EngineError> {
    repo.merge_base(one, two)
        .map_err(|error| unknown_revision(error, value))?
        .ok_or_else(|| {
            EngineError::invalid_argument(format!(
                "`{one}` and `{two}` share no commit, so `{value}` has no merge base; pass `{one}..{two}` to \
                 diff their trees"
            ))
        })
}

/// A revision git cannot resolve is the caller's to fix; any other git failure is the engine's.
fn unknown_revision(error: GitError, value: &str) -> EngineError {
    match error {
        GitError::RevParse { msg, .. } => EngineError::invalid_argument(format!(
            "`{value}` does not name a revision in this repository ({msg}); pass a commit, branch or tag \
             (\"\" for HEAD), `A..B` or `A...B`"
        )),
        other => other.into(),
    }
}

#[cfg(test)]
#[path = "scope_tests.rs"]
mod tests;
