use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path};

use super::{GitError, Repo, resolve_tree};

impl Repo {
    /// Export regular files from a committed tree into a new directory below a private parent.
    ///
    /// The caller owns the scratch directory and cleans up partial output on failure.
    pub fn export_tree(&self, rev: &str, destination: &Path, max_file_bytes: u64) -> Result<(), GitError> {
        let local = self.local();
        let tree = resolve_tree(&local, rev)?;
        let entries = tree.traverse().breadthfirst.files().map_err(|err| GitError::Read {
            what: format!("tree {rev}"),
            msg: err.to_string(),
        })?;
        fs::create_dir(destination).map_err(|err| export_error(destination, err))?;
        for entry in entries {
            if !entry.mode.is_blob() {
                continue;
            }
            let relative = git_path(entry.filepath.as_ref())?;
            let output = destination.join(relative);
            let object = local.find_object(entry.oid).map_err(|err| GitError::Read {
                what: format!("{rev}:{}", relative.display()),
                msg: err.to_string(),
            })?;
            let blob = object.try_into_blob().map_err(|err| GitError::Read {
                what: format!("{rev}:{}", relative.display()),
                msg: err.to_string(),
            })?;
            if blob.data.len() as u64 > max_file_bytes {
                continue;
            }
            let parent = output.parent().ok_or_else(|| export_error(&output, "missing parent"))?;
            fs::create_dir_all(parent).map_err(|err| export_error(parent, err))?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&output)
                .map_err(|err| export_error(&output, err))?;
            file.write_all(&blob.data).map_err(|err| export_error(&output, err))?;
        }
        Ok(())
    }
}

fn git_path(bytes: &[u8]) -> Result<&Path, GitError> {
    #[cfg(unix)]
    let path = {
        use std::os::unix::ffi::OsStrExt;
        Path::new(std::ffi::OsStr::from_bytes(bytes))
    };
    #[cfg(not(unix))]
    let path = {
        let text = std::str::from_utf8(bytes).map_err(|err| GitError::Other(err.to_string()))?;
        let path = Path::new(text);
        if bytes.contains(&b'\\') || bytes.contains(&b':') {
            return Err(export_error(path, "invalid tree path"));
        }
        path
    };
    let safe_parts = bytes
        .split(|byte| *byte == b'/')
        .all(|part| !part.is_empty() && part != b"." && part != b".." && !part.eq_ignore_ascii_case(b".git"));
    if !safe_parts || bytes.contains(&0) || !path.components().all(|part| matches!(part, Component::Normal(_))) {
        return Err(export_error(path, "invalid tree path"));
    }
    Ok(path)
}

fn export_error(path: &Path, error: impl std::fmt::Display) -> GitError {
    GitError::Other(format!("export {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    fn git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.test")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.test")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn export_uses_committed_blobs_and_never_replaces_destination() {
        let scratch = tempfile::tempdir().unwrap();
        let source = scratch.path().join("source");
        fs::create_dir(&source).unwrap();
        git(&source, &["init", "-q"]);
        fs::create_dir(source.join("nested")).unwrap();
        fs::write(source.join("nested/file.rs"), b"committed").unwrap();
        fs::write(source.join("too-large.rs"), [0; 256]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("nested/file.rs", source.join("link.rs")).unwrap();
        git(&source, &["add", "."]);
        git(&source, &["-c", "commit.gpgsign=false", "commit", "-qm", "fixture"]);
        fs::write(source.join("nested/file.rs"), b"dirty").unwrap();
        fs::write(source.join("untracked.rs"), b"untracked").unwrap();

        let repo = Repo::discover(&source).unwrap();
        let output = scratch.path().join("checkout");
        repo.export_tree("HEAD", &output, 64).unwrap();
        assert_eq!(fs::read(output.join("nested/file.rs")).unwrap(), b"committed");
        assert!(!output.join("link.rs").exists());
        assert!(!output.join("too-large.rs").exists());
        assert!(!output.join("untracked.rs").exists());
        assert!(repo.export_tree("HEAD", &output, 64).is_err());
        assert_eq!(fs::read(output.join("nested/file.rs")).unwrap(), b"committed");
    }

    #[test]
    fn rejects_unsafe_tree_paths() {
        for path in [
            b"../escape".as_slice(),
            b"a/./b",
            b"/absolute",
            b".GIT/config",
            b"a\0b",
            b"",
        ] {
            assert!(git_path(path).is_err(), "{path:?}");
        }
        assert_eq!(git_path(b"a/b").unwrap(), Path::new("a/b"));
    }
}
