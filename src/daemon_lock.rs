//! Single-owner daemon lock, pidfile, and the machine-wide live-daemon registry + ceiling.
//!
//! Shared by every daemon family basemind spawns — the comms broker and the
//! shells/rmux daemon (see [`DaemonKind`]) — so one lock discipline, one registry and one ceiling
//! cover all of them. The registry itself is feature-independent; its consumers stay feature-gated.
//!
//! Two safeguards against the process/memory leak class, layered on top of the per-daemon-dir
//! guards a family may already have (for comms: the UDS bind and the store flock):
//!
//! 1. **[`DaemonLock`]** — an exclusive `fs2` flock on `<dir>/daemon.lock`, acquired *before* the
//!    socket bind so a redundant daemon converges (exits 0) without first unlinking the live
//!    daemon's socket. Holding it writes `<dir>/daemon.pid` and a machine-registry entry; both are
//!    removed on `Drop`. This is the authoritative "one daemon per dir" gate, uniform across Unix
//!    and Windows (a real lock, not the Unix-only socket-inode watchdog).
//!
//! 2. **The machine ceiling** — every live daemon registers a pidfile under `<data_home>/daemons/`.
//!    [`count_live_daemons_of`] tallies one kind's holders, pruning dead ones via [`pid_is_live`],
//!    and a spawner (e.g. `ensure_daemon` in `comms::singleton`) refuses to spawn past
//!    [`max_live_daemons`]. The ceiling is applied per kind so one runaway family cannot starve
//!    another. It is keyed on the resolved data home (`BASEMIND_DATA_HOME`), so it is a
//!    **production** ceiling on a shared cache — NOT the test guard. Tests isolate the data home
//!    per process, so a suite's daemons never accumulate here; their leak safety comes from the
//!    bootstrap reaper and the kill-and-wait test harness, not this count.

#![cfg(any(unix, windows))]

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::store_layout::{cache_root, workspace_key};

/// The flock file within a daemon dir. Distinct from a store's `.lock` — this one is held for the
/// daemon-process lifetime and gates the singleton decision before bind. Deliberately not
/// kind-scoped: it is the per-*directory* gate, so two families must own two directories.
const DAEMON_LOCK_FILE: &str = "daemon.lock";
/// The pidfile within a daemon dir naming the current daemon (for a contender's converge log).
const DAEMON_PID_FILE: &str = "daemon.pid";
/// The machine-wide directory of live-daemon pidfiles under the data home.
const DAEMONS_SUBDIR: &str = "daemons";

/// Default machine-wide ceiling, per kind, on concurrently live daemons before a spawn is refused.
/// Well above any legitimate need (one per user is the norm; a handful covers isolated
/// worktrees/data homes), low enough to turn a runaway into a loud error long before it exhausts
/// the process table.
pub const MAX_LIVE_DAEMONS: usize = 8;
/// Env override for [`MAX_LIVE_DAEMONS`]. Zero or unparseable falls back to the default.
pub const MAX_LIVE_DAEMONS_ENV: &str = "BASEMIND_MAX_DAEMONS";

/// The effective ceiling: [`MAX_LIVE_DAEMONS`] unless [`MAX_LIVE_DAEMONS_ENV`] overrides it.
pub fn max_live_daemons() -> usize {
    match std::env::var(MAX_LIVE_DAEMONS_ENV) {
        Ok(raw) => match raw.trim().parse::<usize>() {
            Ok(n) if n > 0 => n,
            _ => MAX_LIVE_DAEMONS,
        },
        Err(_) => MAX_LIVE_DAEMONS,
    }
}

/// Which daemon family a registry entry belongs to. The registry is machine-wide and shared, so a
/// spawner filters by kind before counting against its ceiling or signalling a stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DaemonKind {
    /// The agent-to-agent comms broker (`basemind comms daemon`).
    // Comms is the default so a record written by a pre-registry-kind binary — which has no `kind`
    // field at all — still deserializes instead of being pruned as corrupt. See `read_record`. ~keep
    #[default]
    Comms,
    /// The shells / rmux daemon.
    Shells,
}

impl DaemonKind {
    /// The wire spelling, matching the serde representation. Used for the registry filename and for
    /// operator-facing output (`comms doctor`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Comms => "comms",
            Self::Shells => "shells",
        }
    }
}

impl std::fmt::Display for DaemonKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One live daemon's registry entry. Written to both `<dir>/daemon.pid` and the machine registry
/// file so a contender can name the holder and the ceiling can prune dead ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonRecord {
    /// The daemon process id.
    pub pid: u32,
    /// Which daemon family this entry belongs to.
    #[serde(default)]
    pub kind: DaemonKind,
    /// The directory this daemon owns (for a comms broker, its comms dir).
    // `comms_dir` was this field's name while the registry was comms-only; the alias keeps records
    // written by an older, still-running binary parseable, since `read_record` prunes what it
    // cannot parse and would otherwise deregister every live daemon on upgrade. ~keep
    #[serde(alias = "comms_dir")]
    pub dir: PathBuf,
    /// The daemon's build version.
    pub version: String,
    /// Unix seconds when the daemon acquired the lock.
    pub started_unix: i64,
}

/// The machine-wide daemons directory (`<data_home>/daemons/`). The registry lives here so it is
/// keyed on the resolved data home; the `_in`/`_at` helpers take it explicitly so unit tests inject
/// a temp dir without mutating the process-global `BASEMIND_DATA_HOME`.
fn daemons_dir() -> PathBuf {
    cache_root().join(DAEMONS_SUBDIR)
}

/// The registry pidfile for a daemon dir within a given machine dir: `<machine_dir>/<kind>-<hash>.pid`.
/// The kind prefix scopes the entry to its family, so one family's registration can never clobber
/// another's — and a kind-filtered scan stays correct even for identically-keyed directories.
fn machine_registry_path_in(machine_dir: &Path, kind: DaemonKind, dir: &Path) -> PathBuf {
    machine_dir.join(format!("{}-{}.pid", kind.as_str(), workspace_key(dir)))
}

/// Whether a process id is still live. Unix: `kill(pid, 0)` — `0` (signalable) or `EPERM` (exists,
/// not ours) both mean alive; `ESRCH` means gone. Windows: `OpenProcess` succeeds for a live pid.
#[cfg(unix)]
pub fn pid_is_live(pid: u32) -> bool {
    // SAFETY: `kill` with signal 0 performs only an existence/permission check and sends no signal.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
pub fn pid_is_live(pid: u32) -> bool {
    /// `OpenProcess` access mask: query basic info only.
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    // SAFETY: `OpenProcess`/`CloseHandle` take only primitive arguments and read no caller memory.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle == 0 {
            return false;
        }
        CloseHandle(handle);
        true
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    /// Win32 `OpenProcess` — returns a process handle (0 on failure).
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
    /// Win32 `CloseHandle`.
    fn CloseHandle(handle: isize) -> i32;
}

/// Read a daemon record from a pidfile, or `None` when absent/corrupt. A `None` here deregisters the
/// holder (the caller reaps the file), which is why [`DaemonRecord`]'s shape only ever grows in
/// backward-compatible ways.
fn read_record(path: &Path) -> Option<DaemonRecord> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Every live daemon registered on this machine (under the resolved data home), of any kind, with
/// the pidfiles of any that have died pruned as a side effect. Best-effort: an unreadable registry
/// dir yields empty. Powers `comms doctor`.
pub fn live_daemons() -> Vec<DaemonRecord> {
    live_daemons_in(&daemons_dir())
}

/// [`live_daemons`] restricted to one family. Powers the per-kind ceiling
/// ([`count_live_daemons_of`]) and `comms stop --all`, which addresses holders over a
/// kind-specific protocol and must not touch another family's daemons.
pub fn live_daemons_of(kind: DaemonKind) -> Vec<DaemonRecord> {
    live_daemons_of_in(&daemons_dir(), kind)
}

/// [`live_daemons_of`] over an explicit machine dir. Injected so unit tests use a temp registry.
fn live_daemons_of_in(machine_dir: &Path, kind: DaemonKind) -> Vec<DaemonRecord> {
    let mut live = live_daemons_in(machine_dir);
    live.retain(|record| record.kind == kind);
    live
}

/// [`live_daemons`] over an explicit machine dir. Injected so unit tests use a temp registry.
fn live_daemons_in(machine_dir: &Path) -> Vec<DaemonRecord> {
    let Ok(entries) = std::fs::read_dir(machine_dir) else {
        return Vec::new();
    };
    let mut live = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("pid") {
            continue;
        }
        match read_record(&path) {
            Some(record) if pid_is_live(record.pid) => live.push(record),
            // Missing/corrupt record or a dead holder: reap the stale pidfile so the count is honest.
            _ => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    live
}

/// Count the live daemons of one kind on this machine, pruning dead pidfiles. See [`live_daemons`].
pub fn count_live_daemons_of(kind: DaemonKind) -> usize {
    live_daemons_of(kind).len()
}

/// The single-owner daemon lock: an exclusive flock plus the two pidfiles, released and removed on
/// `Drop`. Acquire it at the very top of the daemon entry point, before binding the socket.
#[derive(Debug)]
pub struct DaemonLock {
    /// Held for the daemon's lifetime; the flock releases when this `File` drops (or the process
    /// dies — the OS releases advisory locks on exit, so a crashed daemon never wedges the lock).
    _lock: File,
    /// `<dir>/daemon.pid`, removed on drop.
    pid_path: PathBuf,
    /// `<data_home>/daemons/<kind>-<hash>.pid`, removed on drop.
    machine_path: PathBuf,
}

/// The result of trying to become the daemon for a directory.
#[derive(Debug)]
pub enum DaemonLockOutcome {
    /// We are now the sole daemon; hold the returned lock for the process lifetime.
    Acquired(DaemonLock),
    /// Another live daemon already holds this directory. Converge: exit 0. The record names it when
    /// the pidfile was readable.
    AlreadyHeld(Option<DaemonRecord>),
}

/// Total time [`contend_for_lock`] waits out a lock that looks held but has no live holder record.
const RELEASE_GRACE: std::time::Duration = std::time::Duration::from_millis(300);
const RELEASE_POLL: std::time::Duration = std::time::Duration::from_millis(10);

/// Try to take the flock on `file`. `None` means it is now ours; `Some(holder)` means a live peer
/// owns it.
///
/// A flock lives on the open file *description*, so a just-released lock can still read as held
/// while a concurrent fork/exec elsewhere in the machine briefly carries a copy of the holder's fd
/// (until the child execs and close-on-exec drops it). `Drop` removes the pidfile before the lock
/// is released, so that window shows up as a lock that is held yet has no record naming a live
/// process. Only then do we poll for [`RELEASE_GRACE`]; a record naming a live pid is a genuine peer
/// and converges immediately, so the common redundant-spawn path stays instant.
fn contend_for_lock(file: &File, pid_path: &Path) -> Option<Option<DaemonRecord>> {
    let deadline = std::time::Instant::now() + RELEASE_GRACE;
    loop {
        if file.try_lock_exclusive().is_ok() {
            return None;
        }
        let record = read_record(pid_path);
        if record.as_ref().is_some_and(|r| pid_is_live(r.pid)) || std::time::Instant::now() >= deadline {
            return Some(record);
        }
        std::thread::sleep(RELEASE_POLL);
    }
}

impl DaemonLock {
    /// Try to acquire single-ownership of `comms_dir` for a comms broker at `version`. Shorthand for
    /// [`acquire_kind`](Self::acquire_kind) with [`DaemonKind::Comms`].
    pub fn acquire(comms_dir: &Path, version: &str) -> std::io::Result<DaemonLockOutcome> {
        Self::acquire_kind(DaemonKind::Comms, comms_dir, version)
    }

    /// Try to acquire single-ownership of `dir` for a `kind` daemon at `version`.
    ///
    /// Non-blocking: the flock is either free (we win and register) or held by a live daemon (we
    /// converge). Because the OS releases the flock when its holder dies, contention always means a
    /// *live* peer — the caller exits 0 rather than racing to reclaim.
    pub fn acquire_kind(kind: DaemonKind, dir: &Path, version: &str) -> std::io::Result<DaemonLockOutcome> {
        Self::acquire_at(kind, dir, version, &daemons_dir())
    }

    /// [`acquire_kind`](Self::acquire_kind) registering into an explicit machine dir. Injected so
    /// tests exercise the lock + registry against a temp dir without touching `BASEMIND_DATA_HOME`.
    pub fn acquire_at(
        kind: DaemonKind,
        dir: &Path,
        version: &str,
        machine_dir: &Path,
    ) -> std::io::Result<DaemonLockOutcome> {
        let lock_path = dir.join(DAEMON_LOCK_FILE);
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        if let Some(holder) = contend_for_lock(&file, &dir.join(DAEMON_PID_FILE)) {
            return Ok(DaemonLockOutcome::AlreadyHeld(holder));
        }

        let record = DaemonRecord {
            pid: std::process::id(),
            kind,
            dir: dir.to_path_buf(),
            version: version.to_string(),
            started_unix: now_unix(),
        };
        let pid_path = dir.join(DAEMON_PID_FILE);
        let machine_path = machine_registry_path_in(machine_dir, kind, dir);
        write_record(&pid_path, &record);
        let _ = std::fs::create_dir_all(machine_dir);
        write_record(&machine_path, &record);

        Ok(DaemonLockOutcome::Acquired(DaemonLock {
            _lock: file,
            pid_path,
            machine_path,
        }))
    }
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        // removing the pidfiles just keeps the registry tidy so a later count needs no liveness prune.
        let _ = std::fs::remove_file(&self.pid_path);
        let _ = std::fs::remove_file(&self.machine_path);
    }
}

/// Write a daemon record atomically (tmp + rename). Best-effort — the lock is already held, so a
/// failure here only degrades a contender's converge log, never correctness.
fn write_record(path: &Path, record: &DaemonRecord) {
    let Ok(bytes) = serde_json::to_vec(record) else {
        return;
    };
    let tmp = path.with_extension(format!("pid.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, &bytes).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Unix seconds now, or `0` if the clock is before the epoch.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_pid_is_live_and_a_bogus_pid_is_not() {
        assert!(pid_is_live(std::process::id()), "our own pid is live");
        // A pid well above any real one on a fresh machine reads as dead.
        assert!(!pid_is_live(0x7FFF_FFFE), "an absent pid is not live");
    }

    #[test]
    fn a_record_written_by_an_older_binary_still_parses_as_a_comms_daemon() {
        let old_shape = br#"{"pid":4242,"comms_dir":"/tmp/old-comms","version":"0.22.0","started_unix":17}"#;
        let record: DaemonRecord = serde_json::from_slice(old_shape).expect("an old-shape record must still parse");
        assert_eq!(record.kind, DaemonKind::Comms, "an untagged record is a comms daemon");
        assert_eq!(
            record.dir,
            PathBuf::from("/tmp/old-comms"),
            "the directory survives the rename"
        );
        assert_eq!(record.pid, 4242);
    }

    #[test]
    fn acquire_admits_one_owner_then_reports_already_held() {
        let machine = tempfile::tempdir().expect("machine tempdir");
        let comms = tempfile::tempdir().expect("comms tempdir");
        let first =
            DaemonLock::acquire_at(DaemonKind::Comms, comms.path(), "9.9.9", machine.path()).expect("first acquire");
        assert!(
            matches!(first, DaemonLockOutcome::Acquired(_)),
            "the first daemon wins the lock"
        );
        match DaemonLock::acquire_at(DaemonKind::Comms, comms.path(), "9.9.9", machine.path()).expect("second acquire")
        {
            DaemonLockOutcome::AlreadyHeld(Some(record)) => {
                assert_eq!(record.pid, std::process::id(), "the holder record names us");
            }
            other => panic!("a second acquire must report AlreadyHeld with the holder, got {other:?}"),
        }
    }

    #[test]
    fn count_prunes_dead_holders_and_counts_live_ones() {
        let machine = tempfile::tempdir().expect("machine tempdir");
        let comms = tempfile::tempdir().expect("comms tempdir");
        let held = DaemonLock::acquire_at(DaemonKind::Comms, comms.path(), "9.9.9", machine.path()).expect("acquire");
        assert!(matches!(held, DaemonLockOutcome::Acquired(_)));
        assert_eq!(
            live_daemons_in(machine.path()).len(),
            1,
            "our live daemon is counted once"
        );

        // Plant a registry entry for a dead pid; the scan must prune it.
        let dead = DaemonRecord {
            pid: 0x7FFF_FFFE,
            kind: DaemonKind::Comms,
            dir: PathBuf::from("/nonexistent"),
            version: "9.9.9".to_string(),
            started_unix: now_unix(),
        };
        let dead_path = machine.path().join("dead.pid");
        write_record(&dead_path, &dead);
        assert_eq!(
            live_daemons_in(machine.path()).len(),
            1,
            "the dead holder is pruned, only the live one counts"
        );
        assert!(!dead_path.exists(), "the dead pidfile was reaped");
    }

    #[test]
    fn the_ceiling_counts_each_kind_separately() {
        let machine = tempfile::tempdir().expect("machine tempdir");
        let comms = tempfile::tempdir().expect("comms tempdir");
        let shells = tempfile::tempdir().expect("shells tempdir");
        let _comms_lock =
            DaemonLock::acquire_at(DaemonKind::Comms, comms.path(), "9.9.9", machine.path()).expect("comms acquire");
        let _shells_lock =
            DaemonLock::acquire_at(DaemonKind::Shells, shells.path(), "9.9.9", machine.path()).expect("shells acquire");

        assert_eq!(live_daemons_in(machine.path()).len(), 2, "both families are registered");
        let comms_only = live_daemons_of_in(machine.path(), DaemonKind::Comms);
        assert_eq!(comms_only.len(), 1, "a shells daemon does not count against comms");
        assert_eq!(comms_only[0].dir, comms.path(), "the comms entry names the comms dir");
        assert_eq!(
            live_daemons_of_in(machine.path(), DaemonKind::Shells).len(),
            1,
            "the shells daemon is counted under its own kind"
        );
    }

    #[test]
    fn a_record_naming_the_removed_agent_kind_is_reaped_without_crashing() {
        let machine = tempfile::tempdir().expect("machine tempdir");
        let path = machine.path().join("legacy.pid");
        let record = |kind: &str| {
            format!(
                r#"{{"pid":{},"kind":"{kind}","dir":"/tmp/x","version":"1","started_unix":1}}"#,
                std::process::id()
            )
        };
        // Control: the same record with a live kind parses, so only the kind can explain the reap.
        std::fs::write(&path, record("comms")).expect("write control record");
        assert!(read_record(&path).is_some(), "fixture is otherwise valid");
        std::fs::write(&path, record("agent")).expect("write legacy record");
        assert!(read_record(&path).is_none(), "the removed kind no longer parses");
        assert!(live_daemons_in(machine.path()).is_empty());
        assert!(!path.exists(), "the unreadable record is reaped like any corrupt one");
    }

    #[test]
    fn drop_releases_the_lock_and_removes_the_registry_entry() {
        let machine = tempfile::tempdir().expect("machine tempdir");
        let comms = tempfile::tempdir().expect("comms tempdir");
        {
            let held =
                DaemonLock::acquire_at(DaemonKind::Comms, comms.path(), "9.9.9", machine.path()).expect("acquire");
            assert!(matches!(held, DaemonLockOutcome::Acquired(_)));
            assert_eq!(live_daemons_in(machine.path()).len(), 1);
        }
        assert_eq!(
            live_daemons_in(machine.path()).len(),
            0,
            "dropping the lock deregisters the daemon"
        );
        match DaemonLock::acquire_at(DaemonKind::Comms, comms.path(), "9.9.9", machine.path())
            .expect("re-acquire after drop")
        {
            DaemonLockOutcome::Acquired(_) => {}
            other => panic!("the lock is free after drop, got {other:?}"),
        }
    }

    /// A concurrent fork/exec carries a copy of a just-dropped lock's fd until it execs, so the lock
    /// reads as held for a few ms with no record behind it. Re-acquiring right after a drop must
    /// still win rather than report a phantom peer.
    #[test]
    fn reacquire_after_drop_wins_despite_concurrent_fork_exec() {
        let machine = tempfile::tempdir().expect("machine tempdir");
        let comms = tempfile::tempdir().expect("comms tempdir");
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let spawners: Vec<_> = (0..4)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        let _ = std::process::Command::new("true").status();
                    }
                })
            })
            .collect();
        let mut phantom_holders = 0;
        for _ in 0..200 {
            match DaemonLock::acquire_at(DaemonKind::Comms, comms.path(), "9.9.9", machine.path()).expect("acquire") {
                DaemonLockOutcome::Acquired(lock) => drop(lock),
                DaemonLockOutcome::AlreadyHeld(_) => phantom_holders += 1,
            }
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for spawner in spawners {
            spawner.join().expect("spawner thread");
        }
        assert_eq!(phantom_holders, 0, "a released lock was reported as held by a peer");
    }
}
