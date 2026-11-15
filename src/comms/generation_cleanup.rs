//! Removal of rendezvous directories no daemon uses any more (ADR-0024 rule 7).
//!
//! A protocol generation is retired by disuse: nothing deletes its directory while a binary of
//! that generation may still start a daemon in it. [`sweep`] removes another generation's
//! directory, or the files releases up to 0.33 keep directly in the comms directory, only when
//! both hold: its `daemon.lock` can be taken, so no daemon is running there, and its newest file is
//! older than the given age, so none has run there lately. It contacts and signals nothing — the
//! lock and the file ages are the whole test — and never looks at this binary's own generation.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use fs2::FileExt;

use crate::daemon_lock::{RENDEZVOUS_GENERATION, is_generation_name};

/// The default age past which an unlocked rendezvous counts as abandoned.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// The daemon lock's file name, in every layout.
const LOCK_FILE: &str = "daemon.lock";
/// What a release up to 0.33 keeps directly in the comms directory. Fixed: those releases are out.
const PRE_GENERATION_ENTRIES: &[&str] = &[
    "comms.sock",
    "daemon.pid",
    "store.fjall",
    "daemon.log",
    "daemon.log.1",
    "http.addr",
    "last-fatal.json",
    LOCK_FILE,
];

/// Which rendezvous a [`Finding`] is about.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Rendezvous {
    /// Another protocol generation's directory, by name (`g2`).
    Generation(String),
    /// The unscoped files of releases up to 0.33.
    PreGeneration,
}

/// What the sweep decided about one rendezvous.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Removed, or under `dry_run` would be.
    Removed,
    /// A daemon holds its lock.
    InUse,
    /// Its newest file is younger than the age limit.
    Recent,
    /// Removal was attempted and failed part-way.
    Failed,
}

/// One rendezvous the sweep looked at.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Finding {
    /// Which rendezvous.
    pub rendezvous: Rendezvous,
    /// The paths it consists of.
    pub paths: Vec<PathBuf>,
    /// Their total size in bytes.
    pub bytes: u64,
    /// Seconds since its newest file changed.
    pub idle_secs: u64,
    /// The decision.
    pub verdict: Verdict,
}

/// Look at every rendezvous under `comms_base` except this binary's generation and remove the
/// abandoned ones. `now` is a parameter so tests can age a directory without touching mtimes.
pub fn sweep(comms_base: &Path, max_age: Duration, dry_run: bool, now: SystemTime) -> Vec<Finding> {
    let own = format!("g{RENDEZVOUS_GENERATION}");
    let mut findings = Vec::new();
    let pre_generation: Vec<PathBuf> = PRE_GENERATION_ENTRIES
        .iter()
        .map(|name| comms_base.join(name))
        .filter(|path| path.symlink_metadata().is_ok())
        .collect();
    if !pre_generation.is_empty() {
        let lock = comms_base.join(LOCK_FILE);
        findings.push(settle(
            Rendezvous::PreGeneration,
            pre_generation,
            &lock,
            max_age,
            dry_run,
            now,
        ));
    }
    let mut generations: Vec<String> = std::fs::read_dir(comms_base)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| is_generation_name(name) && *name != own)
        .collect();
    generations.sort();
    for name in generations {
        let dir = comms_base.join(&name);
        let lock = dir.join(LOCK_FILE);
        findings.push(settle(
            Rendezvous::Generation(name),
            vec![dir],
            &lock,
            max_age,
            dry_run,
            now,
        ));
    }
    findings
}

fn settle(
    rendezvous: Rendezvous,
    paths: Vec<PathBuf>,
    lock_path: &Path,
    max_age: Duration,
    dry_run: bool,
    now: SystemTime,
) -> Finding {
    let mut bytes = 0;
    let mut newest = SystemTime::UNIX_EPOCH;
    for path in &paths {
        measure(path, &mut bytes, &mut newest);
    }
    let idle = now.duration_since(newest).unwrap_or(Duration::ZERO);
    let mut finding = Finding {
        rendezvous,
        paths,
        bytes,
        idle_secs: idle.as_secs(),
        verdict: Verdict::Recent,
    };
    if idle < max_age {
        return finding;
    }
    // The age test comes first so the lock is only ever tried on a rendezvous nothing has used
    // for a long time: a daemon starting at this instant would read a held lock as a running peer.
    let held = match File::options().read(true).write(true).open(lock_path) {
        Ok(file) => match file.try_lock_exclusive() {
            Ok(()) => Some(file),
            Err(_) => {
                finding.verdict = Verdict::InUse;
                return finding;
            }
        },
        // No lock file: no daemon ever locked here, or it is already partly removed.
        Err(_) => None,
    };
    finding.verdict = Verdict::Removed;
    if !dry_run {
        // The lock file goes last, after the handle is closed: Windows cannot delete an open file.
        let mut removed = true;
        for path in &finding.paths {
            removed &= remove_except(path, lock_path);
        }
        drop(held);
        removed &= remove_path(lock_path);
        for path in &finding.paths {
            if path.is_dir() {
                removed &= std::fs::remove_dir(path).is_ok();
            }
        }
        if !removed {
            finding.verdict = Verdict::Failed;
        }
    }
    finding
}

/// Add `path`'s size to `bytes` and raise `newest` to its latest modification, recursively.
/// Symlinks are measured, not followed.
fn measure(path: &Path, bytes: &mut u64, newest: &mut SystemTime) {
    let Ok(meta) = path.symlink_metadata() else {
        return;
    };
    if let Ok(modified) = meta.modified() {
        *newest = (*newest).max(modified);
    }
    if meta.is_dir() {
        for entry in std::fs::read_dir(path).into_iter().flatten().flatten() {
            measure(&entry.path(), bytes, newest);
        }
    } else {
        *bytes += meta.len();
    }
}

/// Remove `path`, leaving `keep` (and so the directory that holds it) in place.
fn remove_except(path: &Path, keep: &Path) -> bool {
    if path == keep {
        return true;
    }
    if !keep.starts_with(path) {
        return remove_path(path);
    }
    // Every entry is attempted even after one fails, so a failure leaves as little as possible.
    let mut removed = true;
    for entry in std::fs::read_dir(path).into_iter().flatten().flatten() {
        removed &= remove_except(&entry.path(), keep);
    }
    removed
}

fn remove_path(path: &Path) -> bool {
    match path.symlink_metadata() {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path).is_ok(),
        Ok(_) => std::fs::remove_file(path).is_ok(),
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    fn generation(base: &Path, name: &str) -> PathBuf {
        let dir = base.join(name);
        std::fs::create_dir_all(dir.join("store.fjall")).expect("mkdir");
        for file in ["daemon.lock", "daemon.pid", "daemon.log", "store.fjall/data"] {
            std::fs::write(dir.join(file), b"12345").expect("write");
        }
        dir
    }

    fn pre_generation(base: &Path) {
        std::fs::create_dir_all(base.join("store.fjall")).expect("mkdir");
        for file in [
            "comms.sock",
            "daemon.lock",
            "daemon.pid",
            "http.addr",
            "store.fjall/data",
        ] {
            std::fs::write(base.join(file), b"12345").expect("write");
        }
    }

    fn other() -> String {
        format!("g{}", RENDEZVOUS_GENERATION + 1)
    }

    /// Far enough ahead that everything written during the test is past the default age.
    fn later() -> SystemTime {
        SystemTime::now() + DEFAULT_MAX_AGE + DAY
    }

    #[test]
    fn an_old_unlocked_generation_and_the_pre_generation_files_are_removed() {
        let base = tempfile::tempdir().expect("tempdir");
        let old = generation(base.path(), &other());
        pre_generation(base.path());
        std::fs::write(base.path().join("unrelated.txt"), b"keep").expect("write");

        let findings = sweep(base.path(), DEFAULT_MAX_AGE, false, later());

        assert_eq!(findings.len(), 2, "{findings:?}");
        assert!(findings.iter().all(|f| f.verdict == Verdict::Removed), "{findings:?}");
        assert_eq!(findings[0].rendezvous, Rendezvous::PreGeneration);
        assert_eq!(findings[0].bytes, 25);
        assert_eq!(findings[1].rendezvous, Rendezvous::Generation(other()));
        assert_eq!(findings[1].bytes, 20);
        assert!(!old.exists(), "the abandoned generation directory is gone");
        let left: Vec<String> = std::fs::read_dir(base.path())
            .expect("read base")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["unrelated.txt"], "only the rendezvous files are removed");
    }

    #[test]
    fn this_generation_is_never_removed_or_reported() {
        let base = tempfile::tempdir().expect("tempdir");
        let own = generation(base.path(), &format!("g{RENDEZVOUS_GENERATION}"));

        let findings = sweep(base.path(), Duration::ZERO, false, later());

        assert_eq!(findings, Vec::new());
        assert!(own.join("store.fjall/data").exists());
    }

    #[test]
    fn a_held_lock_spares_the_rendezvous() {
        let base = tempfile::tempdir().expect("tempdir");
        let busy = generation(base.path(), &other());
        pre_generation(base.path());
        let holders: Vec<File> = [busy.join("daemon.lock"), base.path().join("daemon.lock")]
            .iter()
            .map(|path| {
                let file = File::options().read(true).write(true).open(path).expect("open lock");
                file.try_lock_exclusive().expect("hold the daemon lock");
                file
            })
            .collect();

        let findings = sweep(base.path(), DEFAULT_MAX_AGE, false, later());

        assert_eq!(findings.len(), 2);
        assert!(findings.iter().all(|f| f.verdict == Verdict::InUse), "{findings:?}");
        assert!(busy.join("store.fjall/data").exists());
        assert!(base.path().join("store.fjall/data").exists());
        assert!(base.path().join("comms.sock").exists());
        drop(holders);
    }

    #[test]
    fn a_recently_used_rendezvous_is_spared() {
        let base = tempfile::tempdir().expect("tempdir");
        let young = generation(base.path(), &other());
        pre_generation(base.path());

        let findings = sweep(base.path(), DEFAULT_MAX_AGE, false, SystemTime::now() + DAY);

        assert_eq!(findings.len(), 2);
        assert!(findings.iter().all(|f| f.verdict == Verdict::Recent), "{findings:?}");
        assert!(young.join("daemon.pid").exists());
        assert!(base.path().join("daemon.pid").exists());
    }

    #[test]
    fn a_dry_run_reports_sizes_and_removes_nothing() {
        let base = tempfile::tempdir().expect("tempdir");
        let old = generation(base.path(), &other());

        let findings = sweep(base.path(), DEFAULT_MAX_AGE, true, later());

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].verdict, Verdict::Removed);
        assert_eq!(findings[0].bytes, 20);
        assert!(findings[0].idle_secs >= DEFAULT_MAX_AGE.as_secs());
        assert!(old.join("store.fjall/data").exists(), "a dry run deletes nothing");
    }
}
