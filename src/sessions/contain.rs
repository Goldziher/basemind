//! Containment for the transcript roots (ADR-0049 rules 1, 3 and 4): a root is canonicalized and
//! may not be a filesystem root, a walk follows no symlink, every file found must canonicalize
//! under its root, and a transcript is opened read-only without following a final symlink.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

/// A transcript root as found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootState {
    /// The canonical directory.
    Present(PathBuf),
    Missing,
    /// Refused, with the reason.
    Refused(&'static str),
}

/// Canonicalize `path` as a root. The root itself may be a symlink (a home kept in dotfiles); what
/// it resolves to is the boundary.
pub fn open_root(path: &Path) -> RootState {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return RootState::Missing,
        Err(_) => return RootState::Refused("unreadable"),
        Ok(_) => {}
    }
    let Ok(canonical) = path.canonicalize() else {
        return RootState::Refused("unresolvable");
    };
    if canonical.parent().is_none() {
        return RootState::Refused("a filesystem root");
    }
    if !canonical.is_dir() {
        return RootState::Refused("not a directory");
    }
    RootState::Present(canonical)
}

/// The `*.jsonl` files a walk found, and how many entries it refused.
#[derive(Debug, Default)]
pub struct Found {
    pub files: Vec<PathBuf>,
    /// Symlinks, and files that resolve outside their root.
    pub refused: u64,
}

/// Collect the `*.jsonl` files under `dir` (inside `root`), descending at most `depth` levels and
/// following no symlink.
pub fn walk(root: &Path, dir: &Path, depth: usize, found: &mut Found) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_symlink() {
            found.refused += 1;
        } else if kind.is_dir() {
            if depth > 0 {
                walk(root, &path, depth - 1, found);
            }
        } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "jsonl") {
            match path.canonicalize() {
                Ok(canonical) if canonical.starts_with(root) => found.files.push(canonical),
                _ => found.refused += 1,
            }
        }
    }
}

/// `root/name` when it is a real directory inside `root`; a symlink there is refused.
pub fn child_dir(root: &Path, name: &str, found: &mut Found) -> Option<PathBuf> {
    let path = root.join(name);
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if meta.file_type().is_symlink() {
        found.refused += 1;
        return None;
    }
    meta.is_dir().then_some(path)
}

/// Open a transcript read-only. On Unix the final component is opened with `O_NOFOLLOW`, so a file
/// swapped for a symlink after the walk is refused rather than followed.
pub fn open(path: &Path) -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_filesystem_root_is_refused_and_a_missing_root_is_missing() {
        assert_eq!(open_root(Path::new("/")), RootState::Refused("a filesystem root"));
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(open_root(&dir.path().join("absent")), RootState::Missing);
    }

    #[cfg(unix)]
    #[test]
    fn a_walk_refuses_symlinked_files_and_directories() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.jsonl"), "{}\n").unwrap();
        let root_dir = tempfile::tempdir().unwrap();
        let RootState::Present(root) = open_root(root_dir.path()) else {
            panic!("root not present");
        };
        std::fs::write(root.join("real.jsonl"), "{}\n").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.jsonl"), root.join("link.jsonl")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("linked-dir")).unwrap();
        let mut found = Found::default();
        walk(&root, &root, 3, &mut found);
        assert_eq!(found.files, vec![root.join("real.jsonl")]);
        assert_eq!(found.refused, 2);
        assert!(open(&root.join("link.jsonl")).is_err());
    }
}
