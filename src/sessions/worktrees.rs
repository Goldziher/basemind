//! Removed worktrees (issue #162, ADR-0049): a session that ran in a worktree since removed is
//! still this repository's.
//!
//! A worktree counts as removed when its gitdir is gone and `git worktree list` no longer lists it.
//! Nothing in git records where a removed worktree was, so it is recognised by where it was made:
//! a [`WorktreeDir`] is a directory, no checkout itself, that holds one of the repository's live
//! linked worktrees and no other repository, and a child of it that is no live checkout is a
//! removed worktree when it is gone, or when its `.git` file names a gitdir of this repository that
//! is gone. A child that exists and is not a worktree of this repository (a plain directory, another
//! clone) is not one.
//!
//! The probe stats `<dir>/<name>`, and reads `<dir>/<name>/.git` only when that is a regular file
//! in a real directory, at most [`GITFILE_MAX`] bytes of it.

use std::io::Read;
use std::path::{Path, PathBuf};

use super::Checkout;
use super::lexical::Lexical;

/// The most of a `.git` file read: it holds one `gitdir: <path>` line.
pub const GITFILE_MAX: u64 = 4096;

/// A directory holding linked worktrees of this repository and no other repository.
#[derive(Debug, Clone)]
pub struct WorktreeDir {
    /// Canonical path.
    pub path: PathBuf,
    /// The path as git recorded it for a worktree in it, when it differs (a symlinked home).
    pub alias: Option<PathBuf>,
    /// `path` and `alias`, as transcript paths are compared with them.
    pub(super) lexical: Vec<Lexical>,
}

/// The directories that hold the `linked` worktrees and nothing but `checkouts` and other worktrees
/// of the repository whose canonical common git directory is `common`, deepest first. A checkout is
/// none: a directory gone from it is one of its own. Only a linked worktree whose checkout exists
/// admits its directory: git still lists one whose directory was deleted until it is pruned.
pub fn dirs(linked: &[Checkout], checkouts: &[Checkout], common: &Path) -> Vec<WorktreeDir> {
    let mut dirs: Vec<WorktreeDir> = Vec::new();
    for worktree in linked {
        if !worktree.path.is_dir() {
            continue;
        }
        let Some(path) = worktree.path.parent() else {
            continue;
        };
        let seen = dirs.iter().any(|dir| dir.path == path) || checkouts.iter().any(|c| c.path == path);
        if seen || !holds_only_this_repository(path, common) {
            continue;
        }
        let alias = worktree
            .alias
            .as_deref()
            .and_then(Path::parent)
            .filter(|alias| *alias != path)
            .map(Path::to_path_buf);
        let lexical = std::iter::once(path)
            .chain(alias.as_deref())
            .map(|p| Lexical::parse(&p.to_string_lossy()))
            .collect();
        dirs.push(WorktreeDir {
            path: path.to_path_buf(),
            alias,
            lexical,
        });
    }
    dirs.sort_by_key(|dir| std::cmp::Reverse(dir.path.components().count()));
    dirs
}

/// Whether every child directory of `dir` that is a git checkout is the main checkout of the
/// repository at `common` (its `.git` is that directory) or one of its linked worktrees (its `.git`
/// file names a gitdir of it). A child that cannot be inspected counts against `dir`: only a
/// `.git` that is not there at all is no checkout.
fn holds_only_this_repository(dir: &Path, common: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        // `file_type` does not follow a symlink: a symlinked child is not looked into.
        let Ok(kind) = entry.file_type() else {
            return false;
        };
        if !kind.is_dir() {
            continue;
        }
        let child = entry.path();
        let dot_git = child.join(".git");
        match std::fs::symlink_metadata(&dot_git) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return false,
            Ok(meta) if meta.is_dir() => {
                if dot_git.canonicalize().ok().as_deref() != Some(common) {
                    return false;
                }
            }
            Ok(_) => {
                if !gitdir(&child).is_some_and(|g| of_repository(&g, common)) {
                    return false;
                }
            }
        }
    }
    true
}

/// Whether `dir`, which git lists as no worktree, is a removed worktree of the repository at
/// `common`: gone, or a real directory whose `.git` file names a gitdir of that repository that is
/// gone.
pub fn removed(dir: &Path, common: &Path) -> bool {
    match std::fs::symlink_metadata(dir) {
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
        Ok(meta) => {
            meta.is_dir()
                && gitdir(dir).is_some_and(|g| of_repository(&g, common) && std::fs::symlink_metadata(&g).is_err())
        }
    }
}

/// The gitdir the `.git` file of a linked worktree at `dir` names, taken from `dir` when relative;
/// `None` when `.git` is not a regular file of that shape.
fn gitdir(dir: &Path) -> Option<PathBuf> {
    let dot_git = dir.join(".git");
    if !std::fs::symlink_metadata(&dot_git).ok()?.is_file() {
        return None;
    }
    let mut text = String::new();
    std::fs::File::open(&dot_git)
        .ok()?
        .take(GITFILE_MAX)
        .read_to_string(&mut text)
        .ok()?;
    let named = text.lines().next()?.strip_prefix("gitdir:")?.trim();
    (!named.is_empty()).then(|| dir.join(named))
}

/// Whether `gitdir` is `<common>/worktrees/<name>`. `common` still exists, so it is compared by what
/// it resolves to even when the `worktrees` directory is gone with the last worktree.
fn of_repository(gitdir: &Path, common: &Path) -> bool {
    let Some(worktrees) = gitdir.parent() else {
        return false;
    };
    worktrees.file_name().is_some_and(|name| name == "worktrees")
        && worktrees
            .parent()
            .and_then(|parent| parent.canonicalize().ok())
            .is_some_and(|parent| parent == common)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_gitfile(dir: &Path, gitdir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
    }

    #[test]
    fn only_a_linked_worktree_whose_checkout_exists_admits_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let common = base.join("repo/.git");
        std::fs::create_dir_all(common.join("worktrees/live")).unwrap();
        let wts = base.join("wts");
        std::fs::create_dir_all(&wts).unwrap();
        let admitted = |names: &[&str]| -> Vec<PathBuf> {
            let listed = std::iter::once((base.join("repo"), "main".to_owned()))
                .chain(names.iter().map(|name| (wts.join(name), (*name).to_owned())));
            let scope = super::super::Scope::new(listed);
            let checkouts = scope.checkouts();
            dirs(&checkouts[1..], checkouts, &common)
                .into_iter()
                .map(|found| found.path)
                .collect()
        };
        assert_eq!(
            admitted(&["deleted"]),
            Vec::<PathBuf>::new(),
            "a deleted checkout admits nothing"
        );
        write_gitfile(&wts.join("live"), &common.join("worktrees/live"));
        assert_eq!(admitted(&["deleted", "live"]), std::slice::from_ref(&wts));
    }

    #[test]
    fn a_worktree_is_removed_when_it_is_gone_or_its_gitdir_is() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let common = base.join("repo/.git");
        std::fs::create_dir_all(common.join("worktrees/live")).unwrap();
        std::fs::create_dir_all(base.join("other/.git/worktrees")).unwrap();
        let wts = base.join("wts");
        write_gitfile(&wts.join("live"), &common.join("worktrees/live"));
        write_gitfile(&wts.join("pruned"), &common.join("worktrees/pruned"));
        write_gitfile(&wts.join("relative"), Path::new("../../repo/.git/worktrees/relative"));
        write_gitfile(&wts.join("foreign"), &base.join("other/.git/worktrees/foreign"));
        std::fs::create_dir_all(wts.join("plain")).unwrap();
        let is_removed = |name: &str| removed(&wts.join(name), &common);
        assert!(is_removed("gone"), "the directory is gone");
        assert!(is_removed("pruned"), "its gitdir is gone");
        assert!(is_removed("relative"), "a relative gitdir is taken from the worktree");
        assert!(!is_removed("live"), "its gitdir is there");
        assert!(!is_removed("foreign"), "another repository's gitdir");
        assert!(!is_removed("plain"), "no `.git` file: no worktree");
    }

    /// The directories `dirs` admits when `listed` worktrees sit in `base/wts` beside the main checkout.
    fn admitted_beside(base: &Path, common: &Path, listed: &[&str]) -> Vec<PathBuf> {
        let wts = base.join("wts");
        let all = std::iter::once((base.join("repo"), "main".to_owned()))
            .chain(listed.iter().map(|name| (wts.join(name), (*name).to_owned())));
        let scope = super::super::Scope::new(all);
        let checkouts = scope.checkouts();
        dirs(&checkouts[1..], checkouts, common)
            .into_iter()
            .map(|found| found.path)
            .collect()
    }

    #[test]
    fn a_stale_registration_whose_directory_holds_another_clone_does_not_admit_its_parent() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let common = base.join("repo/.git");
        std::fs::create_dir_all(common.join("worktrees/live")).unwrap();
        let wts = base.join("wts");
        write_gitfile(&wts.join("live"), &common.join("worktrees/live"));
        assert_eq!(admitted_beside(&base, &common, &["live"]), std::slice::from_ref(&wts));
        std::fs::create_dir_all(wts.join("stale/.git")).unwrap();
        assert_eq!(
            admitted_beside(&base, &common, &["live", "stale"]),
            Vec::<PathBuf>::new(),
            "`stale` is registered but now holds another clone"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_child_whose_git_entry_cannot_be_inspected_does_not_admit_its_parent() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let common = base.join("repo/.git");
        std::fs::create_dir_all(common.join("worktrees/live")).unwrap();
        let wts = base.join("wts");
        write_gitfile(&wts.join("live"), &common.join("worktrees/live"));
        let sealed = wts.join("sealed");
        std::fs::create_dir_all(&sealed).unwrap();
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o000)).unwrap();
        let unreadable = std::fs::symlink_metadata(sealed.join(".git"))
            .is_err_and(|error| error.kind() != std::io::ErrorKind::NotFound);
        let admitted = admitted_beside(&base, &common, &["live"]);
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o755)).unwrap();
        if unreadable {
            assert_eq!(admitted, Vec::<PathBuf>::new());
        }
    }
}
