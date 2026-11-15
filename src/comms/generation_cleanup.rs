//! Removal of rendezvous directories no daemon uses any more (ADR-0024 rule 7).
//!
//! A protocol generation is retired by disuse: nothing deletes its directory while a binary of
//! that generation may still start a daemon in it. [`sweep`] removes another generation's
//! directory, or the files releases up to 0.33 keep directly in the comms directory, only when
//! both hold: its `daemon.lock` exists and this process takes it, so no daemon is running there,
//! and its newest file is older than the given age, so none has run there lately. A directory
//! without a `daemon.lock` is not treated as a rendezvous, and a lock that cannot be opened counts
//! as held. It contacts and signals nothing — the lock and the file ages are the whole test — and
//! never looks at this binary's own generation.
//!
//! Removal is made to look atomic to a daemon that starts meanwhile. Still holding the lock, the
//! sweep renames a generation directory to `g<N>.removing-<pid>` and deletes it under that name,
//! so the generation's own name is either the whole old directory or free; for the pre-generation
//! files it renames `store.fjall` aside the same way before deleting anything. The lock file is
//! deleted while it is held, never after it is released. A `*.removing-<pid>` entry left by a
//! sweep that died is deleted by the next one.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use fs2::FileExt;

use crate::daemon_lock::{RENDEZVOUS_GENERATION, is_generation_name, pid_is_live};

/// The default age past which an unlocked rendezvous counts as abandoned.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// The daemon lock's file name, in every layout.
const LOCK_FILE: &str = "daemon.lock";
/// The comms store's directory name, in every layout.
const STORE_DIR: &str = "store.fjall";
/// What follows the original name in an entry a sweep is deleting; the sweep's pid follows it.
const REMOVING_INFIX: &str = ".removing-";
/// What a release up to 0.33 keeps directly in the comms directory. Fixed: those releases are out.
const PRE_GENERATION_ENTRIES: &[&str] = &[
    "comms.sock",
    "daemon.pid",
    STORE_DIR,
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
    /// What a sweep that died part-way left behind, by name (`g2.removing-4242`).
    Leftover(String),
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
    /// It has no `daemon.lock`, so nothing shows it to be a rendezvous; left as it is.
    Skipped,
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
    let mut findings = leftovers(comms_base, dry_run, now);
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
    // Only a `daemon.lock` this process holds proves that no daemon runs here. Without the file
    // there is nothing to hold: the directory may belong to a release that locks another way, or
    // not be a rendezvous at all. A lock that cannot be opened may be held by anyone.
    let held = match File::options().read(true).write(true).open(lock_path) {
        Ok(file) if file.try_lock_exclusive().is_ok() => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            finding.verdict = settle_unlocked(&finding.paths, dry_run);
            return finding;
        }
        Ok(_) | Err(_) => {
            finding.verdict = Verdict::InUse;
            return finding;
        }
    };
    finding.verdict = Verdict::Removed;
    if !dry_run {
        let removed = match &finding.rendezvous {
            Rendezvous::PreGeneration => remove_pre_generation(&finding.paths, lock_path, held),
            Rendezvous::Generation(_) | Rendezvous::Leftover(_) => finding
                .paths
                .first()
                .is_some_and(|dir| remove_generation(dir, lock_path, held)),
        };
        if !removed {
            finding.verdict = Verdict::Failed;
        }
    }
    finding
}

/// Where `path` is moved to be deleted: beside itself, under a name no daemon looks for.
fn aside_of(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!("{REMOVING_INFIX}{}", std::process::id()));
    path.with_file_name(name)
}

/// Move `path` aside, replacing what an earlier sweep with this pid may have left there.
fn move_aside(path: &Path) -> Option<PathBuf> {
    let aside = aside_of(path);
    if aside.symlink_metadata().is_ok() && !remove_path(&aside) {
        return None;
    }
    std::fs::rename(path, &aside).ok().map(|()| aside)
}

/// Delete a generation directory whose lock `held` is. The directory leaves its name in one
/// rename, lock file and all, and is deleted under the new name with the lock still held.
fn remove_generation(dir: &Path, lock_path: &Path, held: File) -> bool {
    let Some(aside) = move_aside(dir) else {
        // Windows may refuse to rename a directory while a file in it is open, and the lock is.
        return cfg!(windows) && remove_in_place(dir, lock_path, held);
    };
    // Where an open file blocks its directory's removal, the second attempt follows the close.
    let removed = std::fs::remove_dir_all(&aside).is_ok();
    drop(held);
    removed || std::fs::remove_dir_all(&aside).is_ok()
}

/// Delete a generation directory where it stands, the lock file last and while it is held.
fn remove_in_place(dir: &Path, lock_path: &Path, held: File) -> bool {
    let mut removed = remove_except(dir, lock_path);
    let unlinked = remove_path(lock_path);
    drop(held);
    removed &= unlinked || remove_path(lock_path);
    removed && std::fs::remove_dir(dir).is_ok()
}

/// Delete the pre-generation files, whose lock `held` is. The store goes first and in one rename:
/// it is the only part a daemon would open and find half there. The lock file is deleted last,
/// while it is still held.
fn remove_pre_generation(paths: &[PathBuf], lock_path: &Path, held: File) -> bool {
    let store = lock_path.with_file_name(STORE_DIR);
    let mut removed = true;
    if store.symlink_metadata().is_ok() {
        let Some(aside) = move_aside(&store) else {
            return false;
        };
        removed &= remove_path(&aside);
    }
    for path in paths.iter().filter(|path| *path != lock_path && **path != store) {
        removed &= remove_path(path);
    }
    let unlinked = remove_path(lock_path);
    drop(held);
    removed && (unlinked || remove_path(lock_path))
}

/// Entries an earlier sweep moved aside and did not finish deleting: `<name>.removing-<pid>` for a
/// generation directory or the pre-generation store, when no process with that pid can still be
/// at it. They are deleted whatever their age: nothing looks for them under that name.
fn leftovers(comms_base: &Path, dry_run: bool, now: SystemTime) -> Vec<Finding> {
    let mut names: Vec<String> = std::fs::read_dir(comms_base)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| is_abandoned_leftover(name))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let path = comms_base.join(&name);
            let (mut bytes, mut newest) = (0, SystemTime::UNIX_EPOCH);
            measure(&path, &mut bytes, &mut newest);
            let removed = dry_run || remove_path(&path);
            Finding {
                rendezvous: Rendezvous::Leftover(name),
                paths: vec![path],
                bytes,
                idle_secs: now.duration_since(newest).unwrap_or(Duration::ZERO).as_secs(),
                verdict: if removed { Verdict::Removed } else { Verdict::Failed },
            }
        })
        .collect()
}

/// Whether `name` is `<generation or store>.removing-<pid>` with no other live process of that pid.
fn is_abandoned_leftover(name: &str) -> bool {
    let Some((original, pid)) = name.split_once(REMOVING_INFIX) else {
        return false;
    };
    let Ok(pid) = pid.parse::<u32>() else {
        return false;
    };
    (is_generation_name(original) || original == STORE_DIR) && (pid == std::process::id() || !pid_is_live(pid))
}

/// The verdict on a rendezvous with no `daemon.lock`: an empty generation directory is removed,
/// since removing an empty directory cannot lose anything; whatever else is there stays.
fn settle_unlocked(paths: &[PathBuf], dry_run: bool) -> Verdict {
    let [dir] = paths else {
        return Verdict::Skipped;
    };
    let empty = std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_none());
    if empty && (dry_run || std::fs::remove_dir(dir).is_ok()) {
        Verdict::Removed
    } else {
        Verdict::Skipped
    }
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
#[path = "generation_cleanup_tests.rs"]
mod tests;
