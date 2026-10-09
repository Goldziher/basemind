//! Safe `rescan` plumbing shared by `basemind rescan` and `basemind admin rescan`.
//!
//! Two concerns live here, both about not racing the machine daemon (the sole index writer):
//!
//! * [`resolve_paths`] turns user spellings (`./a.rs`, an absolute path, `src//b.rs`) into the
//!   repo-relative keys the index uses, and refuses anything that cannot be a rescan target —
//!   escaping the repo, naming the repo root, or neither existing on disk nor indexed.
//! * [`rescan_via_daemon`] ships the scan to the daemon when one is running. The CLI never runs an
//!   unlocked in-process scan next to a live daemon.

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::exit::CliExit;
use crate::path::{RelPath, normalize_query_path};

/// Normalize and validate `raw` rescan targets against `root`, returning absolute paths.
///
/// A target that no longer exists on disk is accepted only when the index still knows it (or, for a
/// deleted directory, anything under it): rescanning a deleted file is how its entry is pruned,
/// whereas a typo should fail loudly instead of reporting a green no-op. Every problem is collected
/// into one exit-`2` error so the user fixes them in a single pass.
pub fn resolve_paths(root: &Path, raw: &[String]) -> Result<Vec<PathBuf>> {
    let mut resolved = Vec::with_capacity(raw.len());
    let mut problems = Vec::new();
    let mut indexed: Option<crate::store::Index> = None;
    for spelling in raw {
        let Some(rel) = normalize_query_path(spelling, root) else {
            problems.push(format!(
                "{spelling:?} is outside the repository root or names the root itself (use --full to rescan everything)"
            ));
            continue;
        };
        let abs = root.join(&rel);
        // ~keep An absolute path outside the repo is only ever a `scan.extra_roots` file, which lives
        // ~keep in the index under its absolute key; anything else outside the root is not a target.
        let external = rel.starts_with('/');
        if (external || !abs.exists()) && !is_indexed(root, &mut indexed, &rel) {
            problems.push(if external {
                format!("{spelling:?} is outside the repository and not an indexed extra root")
            } else {
                format!("{spelling:?} does not exist and is not in the index")
            });
            continue;
        }
        resolved.push(abs);
    }
    if problems.is_empty() {
        return Ok(resolved);
    }
    Err(CliExit::usage(format!(
        "invalid rescan path(s):\n  {}",
        problems.join("\n  ")
    )))
}

/// Whether the working-view index holds `rel` (a file) or anything beneath it (a directory).
/// Loaded once per call, only when a path is missing from disk.
fn is_indexed(root: &Path, cache: &mut Option<crate::store::Index>, rel: &str) -> bool {
    if cache.is_none() {
        *cache = crate::store::Store::open_read_only_no_index(root, crate::store::VIEW_WORKING)
            .ok()
            .map(|store| store.index);
    }
    let Some(index) = cache else {
        return false;
    };
    if index.files.contains_key(&RelPath::from(rel)) {
        return true;
    }
    let prefix = format!("{rel}/");
    index
        .files
        .keys()
        .any(|key| key.as_str().is_some_and(|key| key.starts_with(&prefix)))
}

/// Counts the daemon reports for a forwarded rescan.
#[cfg(all(feature = "comms", any(unix, windows)))]
pub use crate::comms::client::RescanReport;

/// Whether a comms daemon is accepting connections for the current comms dir. Cheap (one `stat`
/// when there is no daemon); never spawns one.
#[cfg(all(feature = "comms", any(unix, windows)))]
pub fn daemon_is_up() -> bool {
    crate::git_history::remote::daemon_is_up()
}

/// Without the `comms` feature there is no daemon to defer to.
#[cfg(not(all(feature = "comms", any(unix, windows))))]
pub fn daemon_is_up() -> bool {
    false
}

/// Forward a rescan of `paths` (`None` = the whole working tree) to the running daemon and wait for
/// its counts. The caller has already established that a daemon is up via [`daemon_is_up`]; a
/// failure here is a real error, never a cue to scan locally.
#[cfg(all(feature = "comms", any(unix, windows)))]
pub fn rescan_via_daemon(root: &Path, paths: Option<Vec<PathBuf>>, full: bool) -> Result<RescanReport> {
    use anyhow::Context as _;

    use crate::comms::client::{CommsClient, scope_context_for};

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    runtime.block_on(async {
        let agent = crate::comms::identity::cli_agent_id(root);
        let (remote, cwd) = scope_context_for(root);
        let mut client = CommsClient::ensure_and_connect(agent, remote, cwd)
            .await
            .map_err(|e| CliExit::unavailable(format!("connect to the basemind daemon: {e}")))?;
        client
            .rescan(root.to_path_buf(), paths, full, true)
            .await
            .map_err(|e| anyhow::anyhow!("daemon rescan failed: {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        std::fs::write(dir.path().join("src/a.rs"), "fn a() {}\n").expect("write");
        dir
    }

    #[test]
    fn should_normalize_dot_relative_and_absolute_spellings() {
        let dir = tree();
        let abs = dir.path().join("src/a.rs").display().to_string();
        let got = resolve_paths(dir.path(), &["./src//a.rs".to_string(), abs]).expect("valid");
        assert_eq!(got, vec![dir.path().join("src/a.rs"); 2]);
    }

    #[test]
    fn should_reject_escaping_and_root_paths_with_the_usage_code() {
        let dir = tree();
        for bad in ["../outside.rs", ".", "/etc/passwd"] {
            let error = resolve_paths(dir.path(), &[bad.to_string()]).expect_err(bad);
            assert_eq!(
                super::super::exit::exit_code_for(&error),
                super::super::exit::USAGE,
                "{bad}"
            );
        }
    }

    #[test]
    fn should_reject_a_path_that_is_neither_on_disk_nor_indexed() {
        let dir = tree();
        let error = resolve_paths(dir.path(), &["src/typo.rs".to_string()]).expect_err("typo");
        assert!(error.to_string().contains("does not exist"), "{error}");
    }
}
