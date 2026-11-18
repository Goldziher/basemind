//! Safe plumbing for `basemind rescan`.
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

/// Normalize and validate `raw` rescan targets against `root`, returning absolute paths. Every
/// problem is reported in one exit-`2` error.
pub fn resolve_paths(root: &Path, raw: &[String]) -> Result<Vec<PathBuf>> {
    crate::engine::scan::resolve_rescan_paths(root, raw).map_err(CliExit::usage)
}

/// Counts the daemon reports for a forwarded rescan.
#[cfg(all(feature = "comms", any(unix, windows)))]
pub use crate::comms::client::RescanReport;

/// Whether a comms daemon is accepting connections for the current comms dir. Cheap (one `stat`
/// when there is no daemon); never spawns one.
#[cfg(all(feature = "comms", any(unix, windows)))]
pub fn daemon_is_up() -> bool {
    crate::comms::singleton::daemon_is_up()
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

    use crate::comms::client::{ClientHello, CommsClient};

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    runtime.block_on(async {
        let mut client = CommsClient::ensure_and_connect(ClientHello::anonymous())
            .await
            .map_err(|e| CliExit::unavailable(format!("connect to the basemind daemon: {e}")))?;
        // Paths given on the command line were named, not watched: their blobs are validated.
        client
            .rescan_as(root.to_path_buf(), paths, full, true, true)
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

    /// An existing file in another directory: its key is `/…` on POSIX and `D:/…` on Windows, and
    /// either form must be refused rather than joined onto the root and rescanned as found.
    #[test]
    fn should_reject_an_existing_absolute_path_outside_the_repository() {
        let dir = tree();
        let outside = tempfile::tempdir().expect("tempdir");
        let file = outside.path().join("elsewhere.rs");
        std::fs::write(&file, "fn x() {}\n").expect("write");
        let error = resolve_paths(dir.path(), &[file.display().to_string()]).expect_err("outside");
        assert!(error.to_string().contains("outside the repository"), "{error}");
    }

    #[test]
    fn should_reject_a_path_that_is_neither_on_disk_nor_indexed() {
        let dir = tree();
        let error = resolve_paths(dir.path(), &["src/typo.rs".to_string()]).expect_err("typo");
        assert!(error.to_string().contains("does not exist"), "{error}");
    }
}
