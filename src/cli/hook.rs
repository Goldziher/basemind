//! `basemind hook install` — a git pre-commit hook that refreshes the staged-index view.
//!
//! The hook is a convenience, never a gate: it exits 0 when `basemind` is not on `PATH` and when the
//! scan fails, so it can never block a commit. Installation resolves the hooks directory through
//! git itself (`git rev-parse --git-path hooks`), which honours `core.hooksPath` and works from a
//! linked worktree (where `.git` is a file, not a directory), and it refuses to overwrite a hook it
//! did not write unless `--force` is given.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::Subcommand;

/// First line after the shebang; identifies a hook this command wrote, so a re-run is idempotent.
const HOOK_MARKER: &str = "# Installed by basemind hook install.";

const HOOK_BODY: &str = "#!/usr/bin/env sh
# Installed by basemind hook install.
# Fail-open: refreshing the index must never block a commit.
command -v basemind >/dev/null 2>&1 || exit 0
basemind scan --staged --quiet || echo \"basemind: staged-index refresh failed; commit continues\" >&2
exit 0
";

#[derive(Subcommand, Debug)]
pub enum HookCmd {
    /// Write a pre-commit hook that runs `basemind scan --staged`. Honours `core.hooksPath`, works
    /// in linked worktrees, and never blocks a commit.
    #[command(after_long_help = crate::cli::help::HOOK_INSTALL)]
    Install {
        /// Replace an existing pre-commit hook that basemind did not write (the old one is kept as
        /// `pre-commit.bak`, or `pre-commit.bak.1`, `.bak.2`, ... when earlier backups exist).
        #[arg(long)]
        force: bool,
    },
}

/// What [`install_into`] did, for the status line.
#[derive(Debug, PartialEq, Eq)]
pub enum Installed {
    Fresh,
    Refreshed,
    ReplacedForeign { backup: PathBuf },
}

pub fn run(root: &Path, cmd: HookCmd) -> Result<()> {
    match cmd {
        HookCmd::Install { force } => {
            let hooks_dir = resolve_hooks_dir(root)?;
            let (hook_path, outcome) = install_into(&hooks_dir, force)?;
            match outcome {
                Installed::Fresh | Installed::Refreshed => {}
                Installed::ReplacedForeign { backup } => {
                    println!("kept the previous hook as {}", backup.display());
                }
            }
            println!("installed pre-commit hook at {}", hook_path.display());
            Ok(())
        }
    }
}

/// Where git will look for hooks for the repository at `root`.
///
/// Asking git is the only answer that is right for `core.hooksPath` (absolute, relative, or
/// `~`-prefixed) and for linked worktrees, whose hooks live in the common git dir.
pub fn resolve_hooks_dir(root: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--git-path", "hooks"])
        .output()
        .context("run `git rev-parse --git-path hooks` (is git installed?)")?;
    if !output.status.success() {
        anyhow::bail!(
            "{} is not inside a git repository: {}",
            root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let printed = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let path = PathBuf::from(printed);
    // git prints a path relative to the directory it ran in, which is `root` because of `-C`.
    Ok(if path.is_absolute() { path } else { root.join(path) })
}

/// Whether the pre-commit hook in `hooks_dir` is the one `hook install` writes.
pub fn is_installed(hooks_dir: &Path) -> bool {
    std::fs::read_to_string(hooks_dir.join("pre-commit")).is_ok_and(|body| body.contains(HOOK_MARKER))
}

/// Write the hook into `hooks_dir`, creating it if needed. A foreign hook is an error without
/// `force`; a hook that merely contains [`HOOK_MARKER`] but differs from [`HOOK_BODY`] (the user added
/// lines) counts as foreign. With `force`, the foreign hook is moved to the first free name of `pre-commit.bak`,
/// `pre-commit.bak.1`, `pre-commit.bak.2`, ..., so no earlier backup is replaced.
pub fn install_into(hooks_dir: &Path, force: bool) -> Result<(PathBuf, Installed)> {
    std::fs::create_dir_all(hooks_dir).with_context(|| format!("create {}", hooks_dir.display()))?;
    let hook_path = hooks_dir.join("pre-commit");
    let outcome = match std::fs::read_to_string(&hook_path) {
        Err(_) if !hook_path.exists() => Installed::Fresh,
        Ok(existing) if existing == HOOK_BODY => Installed::Refreshed,
        _ if force => {
            let backup = free_backup_path(hooks_dir)?;
            std::fs::rename(&hook_path, &backup).with_context(|| format!("back up {}", hook_path.display()))?;
            Installed::ReplacedForeign { backup }
        }
        _ => anyhow::bail!(
            "{} already exists and was not written by basemind; re-run with --force to replace it \
             (the old hook is kept as pre-commit.bak, or pre-commit.bak.N when that exists)",
            hook_path.display()
        ),
    };
    std::fs::write(&hook_path, HOOK_BODY).with_context(|| format!("write {}", hook_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&hook_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook_path, perms)?;
    }
    Ok((hook_path, outcome))
}

/// The first of `pre-commit.bak`, `pre-commit.bak.1`, `pre-commit.bak.2`, ... that names nothing in
/// `hooks_dir`, not even a dangling symlink.
fn free_backup_path(hooks_dir: &Path) -> Result<PathBuf> {
    for n in 0_u32.. {
        let candidate = match n {
            0 => hooks_dir.join("pre-commit.bak"),
            _ => hooks_dir.join(format!("pre-commit.bak.{n}")),
        };
        match std::fs::symlink_metadata(&candidate) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(candidate),
            Err(e) => return Err(e).with_context(|| format!("inspect {}", candidate.display())),
            Ok(_) => {}
        }
    }
    anyhow::bail!("no free pre-commit backup name in {}", hooks_dir.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_install_then_refresh_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = dir.path().join("hooks");
        assert_eq!(install_into(&hooks, false).unwrap().1, Installed::Fresh);
        assert!(is_installed(&hooks));
        assert_eq!(install_into(&hooks, false).unwrap().1, Installed::Refreshed);
    }

    #[test]
    fn should_treat_an_edited_marker_hook_as_foreign() {
        let dir = tempfile::tempdir().unwrap();
        let hook = dir.path().join("pre-commit");
        let edited = format!("{HOOK_BODY}echo my own check\n");
        std::fs::write(&hook, &edited).unwrap();
        let err = install_into(dir.path(), false).unwrap_err();
        assert!(err.to_string().contains("--force"), "{err}");
        assert_eq!(std::fs::read_to_string(&hook).unwrap(), edited, "left untouched");

        let (_, outcome) = install_into(dir.path(), true).unwrap();
        assert_eq!(
            outcome,
            Installed::ReplacedForeign {
                backup: dir.path().join("pre-commit.bak")
            }
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("pre-commit.bak")).unwrap(),
            edited
        );
        assert_eq!(std::fs::read_to_string(&hook).unwrap(), HOOK_BODY);
    }

    #[test]
    fn should_refuse_to_overwrite_a_foreign_hook_without_force() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pre-commit"), "#!/bin/sh\necho mine\n").unwrap();
        let err = install_into(dir.path(), false).unwrap_err();
        assert!(err.to_string().contains("--force"), "{err}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("pre-commit")).unwrap(),
            "#!/bin/sh\necho mine\n"
        );
    }

    #[test]
    fn should_back_up_a_foreign_hook_when_forced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pre-commit"), "#!/bin/sh\necho mine\n").unwrap();
        let (_, outcome) = install_into(dir.path(), true).unwrap();
        assert!(matches!(outcome, Installed::ReplacedForeign { .. }));
        assert!(
            std::fs::read_to_string(dir.path().join("pre-commit.bak"))
                .unwrap()
                .contains("mine")
        );
        assert!(is_installed(dir.path()));
    }

    #[test]
    fn should_keep_every_backup_byte_for_byte_across_repeated_forced_installs() {
        let dir = tempfile::tempdir().unwrap();
        let hook = dir.path().join("pre-commit");
        let first: &[u8] = b"#!/bin/sh\necho first \xff\n";
        let second: &[u8] = b"#!/bin/sh\necho second\n";
        let third: &[u8] = b"#!/bin/sh\necho third\n";

        std::fs::write(&hook, first).unwrap();
        let (_, outcome) = install_into(dir.path(), true).unwrap();
        assert_eq!(
            outcome,
            Installed::ReplacedForeign {
                backup: dir.path().join("pre-commit.bak")
            }
        );

        std::fs::write(&hook, second).unwrap();
        let (_, outcome) = install_into(dir.path(), true).unwrap();
        assert_eq!(
            outcome,
            Installed::ReplacedForeign {
                backup: dir.path().join("pre-commit.bak.1")
            }
        );

        std::fs::write(&hook, third).unwrap();
        let (_, outcome) = install_into(dir.path(), true).unwrap();
        assert_eq!(
            outcome,
            Installed::ReplacedForeign {
                backup: dir.path().join("pre-commit.bak.2")
            }
        );

        assert_eq!(std::fs::read(dir.path().join("pre-commit.bak")).unwrap(), first);
        assert_eq!(std::fs::read(dir.path().join("pre-commit.bak.1")).unwrap(), second);
        assert_eq!(std::fs::read(dir.path().join("pre-commit.bak.2")).unwrap(), third);
        assert_eq!(std::fs::read_to_string(&hook).unwrap(), HOOK_BODY);
    }

    #[cfg(unix)]
    #[test]
    fn should_not_replace_a_dangling_symlink_backup() {
        let dir = tempfile::tempdir().unwrap();
        let dangling = dir.path().join("pre-commit.bak");
        std::os::unix::fs::symlink(dir.path().join("missing"), &dangling).unwrap();
        std::fs::write(dir.path().join("pre-commit"), "#!/bin/sh\necho mine\n").unwrap();

        let (_, outcome) = install_into(dir.path(), true).unwrap();

        assert_eq!(
            outcome,
            Installed::ReplacedForeign {
                backup: dir.path().join("pre-commit.bak.1")
            }
        );
        assert_eq!(std::fs::read_link(&dangling).unwrap(), dir.path().join("missing"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("pre-commit.bak.1")).unwrap(),
            "#!/bin/sh\necho mine\n"
        );
    }

    #[test]
    fn should_resolve_hooks_dir_for_a_plain_repo_and_a_linked_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let git = |cwd: &Path, args: &[&str]| {
            let status = crate::testing::git_command()
                .current_dir(cwd)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .output()
                .unwrap();
            assert!(
                status.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&status.stderr)
            );
        };
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let plain = resolve_hooks_dir(&repo).unwrap();
        assert!(plain.ends_with(".git/hooks"), "{plain:?}");

        let linked = dir.path().join("linked");
        git(&repo, &["worktree", "add", "-q", linked.to_str().unwrap()]);
        let from_linked = resolve_hooks_dir(&linked).unwrap();
        assert_eq!(
            from_linked.canonicalize().unwrap_or(from_linked.clone()),
            plain.canonicalize().unwrap_or(plain.clone())
        );

        git(&repo, &["config", "core.hooksPath", "custom-hooks"]);
        let custom = resolve_hooks_dir(&repo).unwrap();
        assert!(custom.ends_with("custom-hooks"), "{custom:?}");
    }
}
