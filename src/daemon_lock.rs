//! Single-owner daemon lock, pidfile, and the machine-wide live-daemon registry + ceiling.
//!
//! Shared by every daemon family basemind spawns (see [`DaemonKind`]), so one lock discipline, one
//! registry and one ceiling cover all of them. The registry itself is feature-independent; its
//! consumers stay feature-gated.
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
//! 2. **The machine ceiling** — every live daemon registers a pidfile under
//!    `<data_home>/daemons/g<N>/`, `N` being its [`RENDEZVOUS_GENERATION`]. [`live_daemons`] and
//!    [`live_daemons_of`] list this generation's holders only, pruning dead ones via
//!    [`pid_is_live`]; they are what `comms doctor` and `comms stop --all` act on, so neither can
//!    reach another generation's daemon. [`count_live_daemons_of`] tallies one kind's holders
//!    across every generation — this one's directory, the other `g<M>/` directories and the
//!    unscoped `daemons/` that releases up to 0.33 register in — by reading the records, never by
//!    contacting a daemon, and a spawner (e.g. `ensure_daemon` in `comms::singleton`) refuses to
//!    spawn past [`max_live_daemons`]. The ceiling is applied per kind so one runaway family
//!    cannot starve another. It is keyed on the resolved data home (`BASEMIND_DATA_HOME`), so it
//!    is a **production** ceiling on a shared cache — NOT the test guard. Tests isolate the data
//!    home per process, so a suite's daemons never accumulate here; their leak safety comes from
//!    the bootstrap reaper and the kill-and-wait test harness, not this count.

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

/// The protocol generation: which subdirectory of the comms directory
/// (`comms::singleton::generation_dir`) this binary's daemon and clients meet in, and which
/// subdirectory of the machine registry its daemons are listed in. Binaries of different
/// generations never contact each other, so each keeps its own daemon alive for as long as its own
/// sessions need it.
///
/// Deliberately not `PROTO_VER`. A protocol bump is survivable inside a generation: mismatched
/// peers refuse each other by the takeover table without anyone being signalled. A generation bump
/// is not free — the new generation starts with an empty comms store (threads, messages and the
/// agent roster do not carry over) and one more daemon runs until the old sessions end.
///
/// Unrelated to the scan generation and to the index's base generations: this one only names where
/// a daemon and its clients meet.
///
/// Bump it, by one, only when a release must stay invisible to binaries ALREADY RELEASED into the
/// current generation, because they would mishandle its daemon or its files and cannot be patched:
///
/// - a released binary's takeover rule would stop or signal this release's daemon (the reason
///   generation 1 exists: every release up to 0.33 kills any daemon whose version or protocol
///   differs from its own);
/// - the bring-up exchange they rely on changes shape: the length-prefixed msgpack framing, the
///   `Ping` / `Status` / `Stop` requests, or the `pid` / `version` / `proto_ver` fields of
///   `StatusReport`;
/// - the meaning of `daemon.lock` / `daemon.pid`, of a registry record, or of the socket bind as a
///   lock, changes;
/// - the store changes on disk in a way its schema version does not announce, so an older binary
///   would open it instead of failing with `NewerSchema`.
///
/// Do not bump it for a `PROTO_VER` bump, a new request or response variant, or a store schema
/// bump: the first two are refused at the handshake and the third is versioned in the store.
pub const RENDEZVOUS_GENERATION: u32 = 1;

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
    /// The agent-shells daemon of an older release. Never written by this version; kept readable
    /// so a record of that kind still parses instead of being reaped as corrupt. See `read_record`.
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

/// The registry root (`<data_home>/daemons/`), holding one subdirectory per generation. Releases
/// up to 0.33 keep their records directly in it; their scan reads only `*.pid` files there, so a
/// generation subdirectory is invisible to them.
fn daemons_root() -> PathBuf {
    cache_root().join(DAEMONS_SUBDIR)
}

/// This generation's registry directory (`<data_home>/daemons/g<N>/`): where this binary's daemons
/// register and the only place its listings look. Keyed on the resolved data home; the `_in`/`_at`
/// helpers take a directory explicitly so unit tests inject a temp dir without mutating the
/// process-global `BASEMIND_DATA_HOME`.
fn daemons_dir() -> PathBuf {
    generation_registry_in(&daemons_root())
}

/// This generation's registry directory under an explicit registry root.
fn generation_registry_in(root: &Path) -> PathBuf {
    root.join(format!("g{RENDEZVOUS_GENERATION}"))
}

/// Whether `name` names a generation's directory, in the registry or in a comms directory: `g`
/// followed by decimal digits.
pub(crate) fn is_generation_name(name: &str) -> bool {
    name.strip_prefix('g')
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
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

/// The live daemon `<dir>/daemon.pid` names, if any. Only the lock holder writes that file, so a
/// record naming a live pid is the daemon that owns `dir` (or, after a crash and pid reuse, an
/// unrelated process — which is why a caller about to signal must also match the pid against what
/// the daemon reported about itself).
pub fn recorded_holder(dir: &Path) -> Option<DaemonRecord> {
    read_record(&dir.join(DAEMON_PID_FILE)).filter(|record| pid_is_live(record.pid))
}

/// Every live daemon of this generation registered on this machine (under the resolved data home),
/// of any kind, with the pidfiles of any that have died pruned as a side effect. Best-effort: an
/// unreadable registry dir yields empty. Powers `comms doctor`. Daemons of other generations are
/// never listed: the caller would address them over a protocol they may not speak.
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

/// Count the live daemons of one kind on this machine across every generation, for the ceiling.
/// This generation's dead pidfiles are pruned; the others' records are only read.
pub fn count_live_daemons_of(kind: DaemonKind) -> usize {
    count_all_generations_in(&daemons_root(), kind)
}

/// [`count_live_daemons_of`] over an explicit registry root.
fn count_all_generations_in(root: &Path, kind: DaemonKind) -> usize {
    live_daemons_of_in(&generation_registry_in(root), kind).len() + count_other_generations_in(root, kind)
}

/// How many live daemons of one kind other generations have registered: the releases up to 0.33,
/// whose records sit directly in the registry root, and every `g<M>/` but this binary's. They count
/// toward the ceiling and are otherwise out of this binary's reach.
pub fn count_live_daemons_of_other_generations(kind: DaemonKind) -> usize {
    count_other_generations_in(&daemons_root(), kind)
}

/// [`count_live_daemons_of_other_generations`] over an explicit registry root.
fn count_other_generations_in(root: &Path, kind: DaemonKind) -> usize {
    let own = generation_registry_in(root);
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    let mut count = count_recorded_in(root, kind);
    for entry in entries.flatten() {
        let path = entry.path();
        let named_generation = entry.file_name().to_str().is_some_and(is_generation_name);
        if named_generation && path != own && path.is_dir() {
            count += count_recorded_in(&path, kind);
        }
    }
    count
}

/// The live `kind` daemons recorded directly in `dir`. Read-only, unlike [`live_daemons_in`]: a
/// record another generation wrote is that generation's to prune, and one this binary cannot parse
/// may be a shape a later release introduced.
fn count_recorded_in(dir: &Path, kind: DaemonKind) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("pid"))
        .filter_map(|path| read_record(&path))
        .filter(|record| record.kind == kind && pid_is_live(record.pid))
        .count()
}

/// The registry record of the live daemon running as `pid`, whatever its generation or kind: the
/// releases up to 0.33 record directly in the registry root, later ones in a `g<N>/` below it.
/// Read-only. For naming the process that holds something this daemon wants; a pid alone does not
/// say which basemind version it is.
pub fn registered_daemon(pid: u32) -> Option<DaemonRecord> {
    registered_daemon_in(&daemons_root(), pid)
}

/// [`registered_daemon`] over an explicit registry root.
fn registered_daemon_in(root: &Path, pid: u32) -> Option<DaemonRecord> {
    let recorded_in = |dir: &Path| {
        std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("pid"))
            .filter_map(|path| read_record(&path))
            .find(|record| record.pid == pid && pid_is_live(pid))
    };
    recorded_in(root).or_else(|| {
        std::fs::read_dir(root)
            .ok()?
            .flatten()
            .filter(|entry| entry.file_name().to_str().is_some_and(is_generation_name))
            .find_map(|entry| recorded_in(&entry.path()))
    })
}

/// What `<dir>/daemon.lock` says about a daemon in `dir`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockState {
    /// There is no lock file: no daemon of this lock scheme has run here, or the directory is gone.
    Absent,
    /// The lock was taken, and released again at once: no daemon is running here.
    Free,
    /// Another process holds the lock, or the file cannot be opened, so that cannot be ruled out.
    Held,
}

/// Test `<dir>/daemon.lock` by taking it for an instant. A contender starting at that instant
/// reads the lock as held with no live holder recorded, which it waits out (see
/// `contend_for_lock`).
pub fn lock_state(dir: &Path) -> LockState {
    match OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join(DAEMON_LOCK_FILE))
    {
        Ok(file) if file.try_lock_exclusive().is_ok() => {
            let _ = FileExt::unlock(&file);
            LockState::Free
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => LockState::Absent,
        Ok(_) | Err(_) => LockState::Held,
    }
}

/// Remove the registry records of other generations whose daemon is provably gone: the directory
/// it owned no longer exists, or its `daemon.lock` is there and free. Such a record still counts
/// toward the ceiling whenever its pid has been reused by a live process, and the generation that
/// wrote it may never run again to prune it. A record this binary cannot parse stays, and so does
/// one whose directory has no `daemon.lock`: a later release may lock another way. With `dry_run`
/// nothing is removed. Returns how many records were (or would be) removed.
pub fn prune_other_generations_records(dry_run: bool) -> usize {
    prune_other_generations_in(&daemons_root(), dry_run)
}

/// [`prune_other_generations_records`] over an explicit registry root.
fn prune_other_generations_in(root: &Path, dry_run: bool) -> usize {
    let own = generation_registry_in(root);
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    let mut registries = vec![root.to_path_buf()];
    for entry in entries.flatten() {
        let path = entry.path();
        let named_generation = entry.file_name().to_str().is_some_and(is_generation_name);
        if named_generation && path != own && path.is_dir() {
            registries.push(path);
        }
    }
    let mut pruned = 0;
    for registry in registries {
        for entry in std::fs::read_dir(&registry).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("pid") {
                continue;
            }
            let Some(record) = read_record(&path) else {
                continue;
            };
            let gone = !record.dir.exists() || lock_state(&record.dir) == LockState::Free;
            if gone && (dry_run || std::fs::remove_file(&path).is_ok()) {
                pruned += 1;
            }
        }
    }
    pruned
}

/// The single-owner daemon lock: an exclusive flock plus the two pidfiles, released and removed on
/// `Drop`. Acquire it at the very top of the daemon entry point, before binding the socket.
#[derive(Debug)]
pub struct DaemonLock {
    /// Held for the daemon's lifetime and explicitly unlocked after deregistration.
    lock: File,
    /// `<dir>/daemon.pid`, removed on drop.
    pid_path: PathBuf,
    /// `<data_home>/daemons/g<N>/<kind>-<hash>.pid`, removed on drop.
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
            lock: file,
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
        // A forked child may still hold a copy of the open file description until it execs. ~keep
        let _ = FileExt::unlock(&self.lock);
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
    fn a_live_daemon_is_found_by_pid_in_any_generation_and_a_dead_one_is_not() {
        let root = tempfile::tempdir().expect("tempdir");
        let record = |pid: u32, version: &str| {
            serde_json::json!({
                "pid": pid, "kind": "comms", "dir": "/tmp/comms", "version": version, "started_unix": 0,
            })
            .to_string()
        };
        let me = std::process::id();
        assert!(
            registered_daemon_in(root.path(), me).is_none(),
            "an empty registry names nobody"
        );

        std::fs::write(root.path().join("comms-dead.pid"), record(0x7FFF_FFFE, "0.33.0")).expect("write");
        assert!(
            registered_daemon_in(root.path(), 0x7FFF_FFFE).is_none(),
            "a dead pid is nobody"
        );

        let other = root.path().join("g7");
        std::fs::create_dir_all(&other).expect("mkdir generation");
        std::fs::write(other.join("comms-x.pid"), record(me, "0.99.0")).expect("write");
        let found = registered_daemon_in(root.path(), me).expect("a record in another generation");
        assert_eq!(found.version, "0.99.0");

        std::fs::write(root.path().join("comms-old.pid"), record(me, "0.33.0")).expect("write");
        let found = registered_daemon_in(root.path(), me).expect("a record in the registry root");
        assert_eq!(found.version, "0.33.0", "the pre-generation layout is read too");
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

    /// A registry record as a release up to 0.33 writes it.
    fn comms_record(pid: u32, dir: &str) -> String {
        format!(r#"{{"pid":{pid},"kind":"comms","dir":"{dir}","version":"0.33.0","started_unix":1}}"#)
    }

    /// What a release up to 0.33 sees of the registry: the `*.pid` files directly in its root.
    fn flat_scan(root: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(root)
            .expect("read registry root")
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("pid"))
            .collect()
    }

    #[test]
    fn this_generations_daemons_register_where_a_pre_generation_scan_cannot_see_them() {
        assert_eq!(
            daemons_dir(),
            daemons_root().join(format!("g{RENDEZVOUS_GENERATION}")),
            "the registry this binary writes and lists is its generation's subdirectory"
        );

        let root = tempfile::tempdir().expect("registry root");
        let comms = tempfile::tempdir().expect("comms tempdir");
        let own = generation_registry_in(root.path());
        let held = DaemonLock::acquire_at(DaemonKind::Comms, comms.path(), "9.9.9", &own).expect("acquire");
        assert!(matches!(held, DaemonLockOutcome::Acquired(_)));

        assert_eq!(live_daemons_in(&own).len(), 1, "registered in its own generation");
        assert_eq!(
            flat_scan(root.path()),
            Vec::<PathBuf>::new(),
            "nothing a release up to 0.33 would list, stop or prune"
        );
    }

    #[test]
    fn listings_cover_this_generation_only_and_the_ceiling_counts_all_of_them() {
        let root = tempfile::tempdir().expect("registry root");
        let comms = tempfile::tempdir().expect("comms tempdir");
        let own = generation_registry_in(root.path());
        let _held = DaemonLock::acquire_at(DaemonKind::Comms, comms.path(), "9.9.9", &own).expect("acquire");

        let other = root.path().join(format!("g{}", RENDEZVOUS_GENERATION + 6));
        std::fs::create_dir_all(&other).expect("another generation's registry");
        let not_a_generation = root.path().join("gone");
        std::fs::create_dir_all(&not_a_generation).expect("unrelated directory");
        let pre_generation = root.path().join("comms-flat.pid");
        let other_live = other.join("comms-live.pid");
        let other_dead = other.join("comms-dead.pid");
        let other_unreadable = other.join("comms-newer-shape.pid");
        let live = std::process::id();
        std::fs::write(&pre_generation, comms_record(live, "/tmp/pre-generation")).expect("write");
        std::fs::write(&other_live, comms_record(live, "/tmp/other-generation")).expect("write");
        std::fs::write(&other_dead, comms_record(0x7FFF_FFFE, "/tmp/dead")).expect("write");
        std::fs::write(&other_unreadable, br#"{"pid":"a later shape"}"#).expect("write");
        std::fs::write(not_a_generation.join("comms-x.pid"), comms_record(live, "/tmp/x")).expect("write");

        let listed = live_daemons_of_in(&own, DaemonKind::Comms);
        assert_eq!(listed.len(), 1, "only this generation's daemon is listed: {listed:?}");
        assert_eq!(listed[0].dir, comms.path());
        assert_eq!(count_other_generations_in(root.path(), DaemonKind::Comms), 2);
        assert_eq!(
            count_all_generations_in(root.path(), DaemonKind::Comms),
            3,
            "this generation's, the pre-generation one and the other generation's live one"
        );
        assert_eq!(count_all_generations_in(root.path(), DaemonKind::Shells), 0);
        for kept in [&pre_generation, &other_live, &other_dead, &other_unreadable] {
            assert!(kept.exists(), "{} is another generation's to prune", kept.display());
        }
    }

    #[test]
    fn the_lock_state_tells_absent_free_and_held_apart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let machine = tempfile::tempdir().expect("machine tempdir");
        assert_eq!(lock_state(dir.path()), LockState::Absent);
        assert_eq!(lock_state(&dir.path().join("missing")), LockState::Absent);

        let held = DaemonLock::acquire_at(DaemonKind::Comms, dir.path(), "9.9.9", machine.path()).expect("acquire");
        assert!(matches!(held, DaemonLockOutcome::Acquired(_)));
        assert_eq!(lock_state(dir.path()), LockState::Held);

        drop(held);
        assert_eq!(lock_state(dir.path()), LockState::Free);
        let again = DaemonLock::acquire_at(DaemonKind::Comms, dir.path(), "9.9.9", machine.path()).expect("acquire");
        assert!(
            matches!(again, DaemonLockOutcome::Acquired(_)),
            "testing the lock does not keep it"
        );
    }

    /// A registry record whose pid was reused counts as a live daemon for good, unless something
    /// that can prove the daemon gone removes it.
    #[test]
    fn records_of_other_generations_are_pruned_only_when_their_daemon_is_provably_gone() {
        let root = tempfile::tempdir().expect("registry root");
        let dirs = tempfile::tempdir().expect("daemon dirs");
        let other = root.path().join(format!("g{}", RENDEZVOUS_GENERATION + 3));
        let own = generation_registry_in(root.path());
        std::fs::create_dir_all(&other).expect("another generation's registry");
        std::fs::create_dir_all(&own).expect("this generation's registry");
        let live = std::process::id();
        let daemon_dir = |name: &str, lock: bool| {
            let dir = dirs.path().join(name);
            std::fs::create_dir_all(&dir).expect("daemon dir");
            if lock {
                std::fs::write(dir.join(DAEMON_LOCK_FILE), b"").expect("lock file");
            }
            dir
        };
        let record = |registry: &Path, name: &str, dir: &Path| {
            let path = registry.join(format!("comms-{name}.pid"));
            std::fs::write(&path, comms_record(live, &dir.to_string_lossy())).expect("write record");
            path
        };
        let unlocked = daemon_dir("unlocked", true);
        let running = daemon_dir("running", true);
        let other_scheme = daemon_dir("other-scheme", false);
        let holder = OpenOptions::new()
            .read(true)
            .write(true)
            .open(running.join(DAEMON_LOCK_FILE))
            .expect("open lock");
        holder.try_lock_exclusive().expect("hold the running daemon's lock");

        let dir_gone = record(root.path(), "dir-gone", &dirs.path().join("removed"));
        let lock_free = record(&other, "lock-free", &unlocked);
        let lock_held = record(&other, "lock-held", &running);
        let no_lock = record(&other, "no-lock", &other_scheme);
        let ours = record(&own, "ours", &dirs.path().join("removed"));
        let unreadable = other.join("comms-newer-shape.pid");
        std::fs::write(&unreadable, br#"{"pid":"a later shape"}"#).expect("write");
        assert_eq!(count_other_generations_in(root.path(), DaemonKind::Comms), 4);

        assert_eq!(prune_other_generations_in(root.path(), true), 2, "a dry run counts");
        assert!(dir_gone.exists() && lock_free.exists(), "and removes nothing");

        assert_eq!(prune_other_generations_in(root.path(), false), 2);
        assert!(!dir_gone.exists(), "its directory is gone");
        assert!(!lock_free.exists(), "its lock is free");
        assert!(lock_held.exists(), "a held lock is a running daemon");
        assert!(no_lock.exists(), "no daemon.lock proves nothing");
        assert!(unreadable.exists(), "an unreadable record may be a later shape");
        assert!(ours.exists(), "this generation prunes its own records by pid");
        assert_eq!(count_other_generations_in(root.path(), DaemonKind::Comms), 2);
        drop(holder);
    }

    #[test]
    fn a_record_an_older_release_wrote_for_a_shells_daemon_still_parses_and_is_kept() {
        let machine = tempfile::tempdir().expect("machine tempdir");
        let path = machine.path().join("shells-legacy.pid");
        let record = format!(
            r#"{{"pid":{},"kind":"shells","dir":"/tmp/x","version":"0.33.0","started_unix":1}}"#,
            std::process::id()
        );
        std::fs::write(&path, record).expect("write legacy record");

        let parsed = read_record(&path).expect("a shells record from an older release still parses");
        assert_eq!(parsed.kind, DaemonKind::Shells);
        assert_eq!(live_daemons_of_in(machine.path(), DaemonKind::Shells).len(), 1);
        assert!(live_daemons_of_in(machine.path(), DaemonKind::Comms).is_empty());
        assert!(path.exists(), "a live legacy daemon's record is not reaped as corrupt");
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
        let reacquired =
            DaemonLock::acquire_at(DaemonKind::Comms, comms.path(), "9.9.9", machine.path()).expect("re-acquire");
        assert!(matches!(reacquired, DaemonLockOutcome::Acquired(_)));
    }

    #[cfg(unix)]
    #[test]
    fn drop_unlocks_even_if_the_open_file_description_is_shared() {
        let machine = tempfile::tempdir().expect("machine tempdir");
        let comms = tempfile::tempdir().expect("comms tempdir");
        let held = DaemonLock::acquire_at(DaemonKind::Comms, comms.path(), "9.9.9", machine.path()).expect("acquire");
        let DaemonLockOutcome::Acquired(held) = held else {
            panic!("must acquire a fresh lock");
        };
        let inherited = held.lock.try_clone().expect("duplicate the lock fd");
        drop(held);
        let reacquired =
            DaemonLock::acquire_at(DaemonKind::Comms, comms.path(), "9.9.9", machine.path()).expect("re-acquire");
        assert!(matches!(reacquired, DaemonLockOutcome::Acquired(_)));
        drop(inherited);
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
