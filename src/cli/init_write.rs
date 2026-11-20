//! Crash-safe file writes for `basemind init`.
//!
//! Every file `init` touches (the rules file, `.claude/settings*.json`, `basemind.toml`,
//! `.gitignore`) is written to a temporary file beside its resolved location and renamed over it,
//! so a crash or a failed write leaves the original intact rather than truncated. A symlinked
//! target is written through, keeping the link, and reported; a target that resolves outside the
//! repository is refused unless the caller allows it (`--yes`).

use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// A completed write: the path `init` was asked to write and, when that path resolves elsewhere
/// through a symlink, the file that was actually replaced.
#[derive(Debug)]
pub(crate) struct Written {
    /// The target as `init` planned it.
    pub(crate) path: PathBuf,
    /// The file the write landed in, when it differs from `path` because of a symlink.
    pub(crate) symlink_target: Option<PathBuf>,
}

impl fmt::Display for Written {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.path.display())?;
        match &self.symlink_target {
            Some(target) => write!(f, " (via symlink → {})", target.display()),
            None => Ok(()),
        }
    }
}

/// Replace `path` with `contents` atomically. A symlinked `path` is written through: the file it
/// resolves to is replaced and the link is kept. A resolved location outside `root` is refused
/// unless `allow_outside`. An existing file keeps its permissions; a new one gets the permissions
/// a plain create would give it.
pub(crate) fn write_file(root: &Path, path: &Path, contents: &[u8], allow_outside: bool) -> Result<Written> {
    let canonical_root = std::fs::canonicalize(root).with_context(|| format!("resolve {}", root.display()))?;
    let resolved = resolve_target(path)?;
    if !allow_outside && !resolved.starts_with(&canonical_root) {
        anyhow::bail!(
            "refusing to write {}: it resolves to {}, outside the repository; re-run with --yes to write there",
            path.display(),
            resolved.display()
        );
    }
    replace(&resolved, |file| file.write_all(contents)).with_context(|| format!("write {}", path.display()))?;
    let expected = path.strip_prefix(root).ok().map(|rel| canonical_root.join(rel));
    let redirected = expected.as_deref() != Some(resolved.as_path());
    Ok(Written {
        path: path.to_path_buf(),
        symlink_target: redirected.then_some(resolved),
    })
}

/// The absolute location a write to `path` lands in, with every symlink on the way resolved. A
/// dangling symlink resolves to the file it names, which the write then creates.
fn resolve_target(path: &Path) -> Result<PathBuf> {
    let is_link = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta.file_type().is_symlink(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(anyhow::Error::new(e).context(format!("inspect {}", path.display()))),
    };
    if !is_link {
        return canonical_in_parent(path);
    }
    match std::fs::canonicalize(path) {
        Ok(resolved) => Ok(resolved),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let dest = std::fs::read_link(path).with_context(|| format!("read symlink {}", path.display()))?;
            let dest = match path.parent() {
                Some(parent) if dest.is_relative() => parent.join(dest),
                _ => dest,
            };
            canonical_in_parent(&dest)
        }
        Err(e) => Err(anyhow::Error::new(e).context(format!("resolve {}", path.display()))),
    }
}

/// `path`'s file name joined onto its canonicalized parent directory, which must exist.
fn canonical_in_parent(path: &Path) -> Result<PathBuf> {
    let name = path
        .file_name()
        .with_context(|| format!("{} names no file", path.display()))?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let dir = std::fs::canonicalize(parent).with_context(|| format!("resolve {}", parent.display()))?;
    Ok(dir.join(name))
}

/// Fill a temporary file beside `target` with `fill`, flush it to disk and rename it over
/// `target`. On any failure the temporary file is removed and `target` is untouched.
fn replace(target: &Path, fill: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>) -> Result<()> {
    let dir = target
        .parent()
        .with_context(|| format!("{} has no parent directory", target.display()))?;
    let existing = match std::fs::metadata(target) {
        Ok(meta) => Some(meta.permissions()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(anyhow::Error::new(e).context(format!("inspect {}", target.display()))),
    };
    let mut builder = tempfile::Builder::new();
    builder.prefix(".basemind-init-").suffix(".tmp");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if existing.is_none() {
            // ~keep The mode a plain create asks for; the umask narrows it exactly as it would there.
            builder.permissions(std::fs::Permissions::from_mode(0o666));
        }
    }
    let mut tmp = builder
        .tempfile_in(dir)
        .with_context(|| format!("create a temporary file in {}", dir.display()))?;
    fill(tmp.as_file_mut()).context("write the temporary file")?;
    tmp.as_file().sync_all().context("flush the temporary file")?;
    if let Some(permissions) = existing {
        tmp.as_file()
            .set_permissions(permissions)
            .context("copy permissions to the temporary file")?;
    }
    tmp.persist(target)
        .map_err(|e| e.error)
        .with_context(|| format!("rename the temporary file over {}", target.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read dir")
            .map(|entry| entry.expect("dir entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_failed_write_leaves_the_original_intact_and_no_temporary_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let target = tmp.path().join("CLAUDE.md");
        std::fs::write(&target, "# original\n").expect("seed CLAUDE.md");

        let result = replace(&target, |file| {
            file.write_all(b"# trunc")?;
            Err(std::io::Error::other("interrupted"))
        });

        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&target).expect("read"), "# original\n");
        assert_eq!(dir_entries(tmp.path()), vec!["CLAUDE.md".to_string()]);
    }

    #[test]
    fn writes_a_plain_target_and_reports_no_symlink() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let target = root.join("AGENTS.md");
        std::fs::write(&target, "# old\n").expect("seed AGENTS.md");

        let written = write_file(root, &target, b"# new\n", false).expect("write");

        assert_eq!(std::fs::read_to_string(&target).expect("read"), "# new\n");
        assert!(written.symlink_target.is_none());
        assert_eq!(written.to_string(), target.display().to_string());
        assert_eq!(dir_entries(root), vec!["AGENTS.md".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_file_keeps_its_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let target = root.join(".gitignore");
        std::fs::write(&target, "target/\n").expect("seed .gitignore");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).expect("chmod");

        write_file(root, &target, b"target/\nCLAUDE.local.md\n", false).expect("write");

        let mode = std::fs::metadata(&target).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
        assert_eq!(
            std::fs::read_to_string(&target).expect("read"),
            "target/\nCLAUDE.local.md\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_new_file_gets_the_permissions_a_plain_create_gives() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let plain = root.join("plain.md");
        std::fs::write(&plain, "").expect("plain create");
        let target = root.join("CLAUDE.local.md");

        write_file(root, &target, b"# rules\n", false).expect("write");

        let mode = |p: &Path| std::fs::metadata(p).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode(&target), mode(&plain));
        assert_eq!(std::fs::read_to_string(&target).expect("read"), "# rules\n");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_target_inside_the_repository_is_written_through_and_reported() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir(root.join("docs")).expect("mkdir docs");
        let real = root.join("docs").join("rules.md");
        std::fs::write(&real, "# old\n").expect("seed rules.md");
        let link = root.join("CLAUDE.md");
        std::os::unix::fs::symlink("docs/rules.md", &link).expect("symlink");

        let written = write_file(root, &link, b"# new\n", false).expect("write");

        let canonical_real = std::fs::canonicalize(&real).expect("canonicalize");
        assert_eq!(
            written.to_string(),
            format!("{} (via symlink → {})", link.display(), canonical_real.display())
        );
        assert!(
            std::fs::symlink_metadata(&link)
                .expect("lstat")
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&real).expect("read"), "# new\n");
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_creates_the_file_it_names() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let link = root.join("AGENTS.md");
        std::os::unix::fs::symlink("shared.md", &link).expect("symlink");

        let written = write_file(root, &link, b"# shared\n", false).expect("write");

        let real = std::fs::canonicalize(root).expect("canonicalize").join("shared.md");
        assert_eq!(written.symlink_target, Some(real.clone()));
        assert_eq!(std::fs::read_to_string(&real).expect("read"), "# shared\n");
    }

    #[cfg(unix)]
    #[test]
    fn a_target_outside_the_repository_is_refused_without_yes_and_written_with_it() {
        let repo = tempfile::tempdir().expect("repo");
        let elsewhere = tempfile::tempdir().expect("elsewhere");
        let root = repo.path();
        let outside = elsewhere.path().join("CLAUDE.md");
        std::fs::write(&outside, "# outside\n").expect("seed outside file");
        let link = root.join("CLAUDE.md");
        std::os::unix::fs::symlink(&outside, &link).expect("symlink");
        let canonical_outside = std::fs::canonicalize(&outside).expect("canonicalize");

        let err = write_file(root, &link, b"# new\n", false).expect_err("refused");

        assert_eq!(
            err.to_string(),
            format!(
                "refusing to write {}: it resolves to {}, outside the repository; re-run with --yes to write there",
                link.display(),
                canonical_outside.display()
            )
        );
        assert_eq!(std::fs::read_to_string(&outside).expect("read"), "# outside\n");

        let written = write_file(root, &link, b"# new\n", true).expect("allowed");

        assert_eq!(written.symlink_target, Some(canonical_outside));
        assert_eq!(std::fs::read_to_string(&outside).expect("read"), "# new\n");
    }
}
