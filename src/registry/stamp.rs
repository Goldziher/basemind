//! Freshness stamps of a clone's branch and worktree state, so a registry query can tell whether
//! its rows still match the repository without enumerating it.
//!
//! Git writes every ref, `HEAD` and `packed-refs` through a lock file renamed into place, and
//! `git worktree add` / `remove` / `move` create, delete or rewrite `<common_dir>/worktrees/<name>`.
//! Each of those changes the modification time of the file or of the directory it lands in, so the
//! metadata of a handful of paths under the common git directory changes whenever a branch or
//! worktree row would.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How recent a stamped modification time may be before the stamp is not trusted. A filesystem
/// keeps modification times at a granularity (one tick on Linux, up to two seconds on FAT), and a
/// change in the same tick as a stamped write leaves the stamp unchanged: a stamp whose newest
/// time is within this margin of when it was taken says nothing about a change that follows it.
pub(super) const RACY_MARGIN: Duration = Duration::from_secs(2);

/// Modification time and length of one path, `None` when it does not exist.
type Meta = Option<(SystemTime, u64)>;

/// The stamped metadata of the paths that change with a clone's branches and worktrees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RepoStamp {
    entries: Vec<(PathBuf, Meta)>,
}

impl RepoStamp {
    /// Stamp the clone whose common git directory is `common_dir`: its `HEAD`, `packed-refs`,
    /// `config` (the remote names the repo id) and reftable list, every directory under
    /// `refs/heads`, and `worktrees/` with each linked worktree's directory, `HEAD` and `gitdir`.
    pub(super) fn of(common_dir: &Path) -> Self {
        let mut entries = Vec::new();
        for file in ["HEAD", "packed-refs", "config", "reftable/tables.list"] {
            push(&mut entries, common_dir.join(file));
        }
        push_dirs(&mut entries, &common_dir.join("refs").join("heads"));
        let worktrees = common_dir.join("worktrees");
        push(&mut entries, worktrees.clone());
        for linked in sorted_subdirs(&worktrees) {
            push(&mut entries, linked.join("HEAD"));
            push(&mut entries, linked.join("gitdir"));
            push(&mut entries, linked);
        }
        Self { entries }
    }

    /// Whether every stamped time is older than `margin` before `taken_at`, so a later change to
    /// any stamped path is bound to move its time.
    pub(super) fn settled(&self, taken_at: SystemTime, margin: Duration) -> bool {
        let Some(cutoff) = taken_at.checked_sub(margin) else {
            return false;
        };
        self.entries
            .iter()
            .filter_map(|(_, meta)| meta.map(|(modified, _)| modified))
            .all(|modified| modified < cutoff)
    }
}

/// Append `path`'s metadata.
fn push(entries: &mut Vec<(PathBuf, Meta)>, path: PathBuf) {
    let meta = std::fs::metadata(&path)
        .ok()
        .and_then(|meta| Some((meta.modified().ok()?, meta.len())));
    entries.push((path, meta));
}

/// Append `dir` and every directory beneath it: a loose ref renamed into place, or deleted, moves
/// the time of the directory that holds it.
fn push_dirs(entries: &mut Vec<(PathBuf, Meta)>, dir: &Path) {
    push(entries, dir.to_path_buf());
    for child in sorted_subdirs(dir) {
        push_dirs(entries, &child);
    }
}

/// The subdirectories of `dir`, sorted so two stamps of one state compare equal.
fn sorted_subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = read
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect();
    dirs.sort();
    dirs
}
