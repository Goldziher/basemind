//! Single-owner daemon lock, pidfile, and the machine-wide live-daemon registry + ceiling.
//!
//! Shared by every daemon family basemind spawns (see `DaemonKind`), so one lock discipline, one
//! registry and one ceiling cover all of them. The registry itself is feature-independent; its
//! consumers stay feature-gated.
//!
//! Two safeguards against the process/memory leak class, layered on top of the per-daemon-dir
//! guards a family may already have (for comms: the UDS bind and the store flock):
//!
//! 1. **`DaemonLock`** — an exclusive `fs2` flock on `<dir>/daemon.lock`, acquired *before* the
//!    socket bind so a redundant daemon converges (exits 0) without first unlinking the live
//!    daemon's socket. Holding it writes `<dir>/daemon.pid` and a machine-registry entry; both are
//!    removed on `Drop`. This is the authoritative "one daemon per dir" gate, uniform across Unix
//!    and Windows (a real lock, not the Unix-only socket-inode watchdog).
//!
//! 2. **The machine ceiling** — every live daemon registers a pidfile under
//!    `<data_home>/daemons/g<N>/`, `N` being its `RENDEZVOUS_GENERATION`. `live_daemons` and
//!    `live_daemons_of` list this generation's holders only, pruning dead ones via
//!    `crate::boot_identity::PidState::may_be_live`; they are what `comms doctor` and `comms stop --all` act on, so neither can
//!    reach another generation's daemon. `count_live_daemons_of` tallies one kind's holders
//!    across every generation — this one's directory, the other `g<M>/` directories and the
//!    unscoped `daemons/` that releases up to 0.33 register in — by reading the records, never by
//!    contacting a daemon, and a spawner (e.g. `ensure_daemon` in `comms::singleton`) refuses to
//!    spawn past `max_live_daemons`. The ceiling is applied per kind so one runaway family
//!    cannot starve another. It is keyed on the resolved data home (`BASEMIND_DATA_HOME`), so it
//!    is a **production** ceiling on a shared cache — NOT the test guard. Tests isolate the data
//!    home per process, so a suite's daemons never accumulate here; their leak safety comes from
//!    the bootstrap reaper and the kill-and-wait test harness, not this count.

#![cfg(any(unix, windows))]

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::data_root::DataRoot;
use crate::store_layout::workspace_key;

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

/// The effective ceiling: the engine configuration's
/// ([`crate::config::engine::EngineConfig::max_live_daemons`]).
pub fn max_live_daemons() -> usize {
    crate::config::engine::with(|config| config.max_live_daemons)
}

/// A [`MAX_LIVE_DAEMONS_ENV`] value; zero or unparseable falls back to [`MAX_LIVE_DAEMONS`].
pub fn max_live_daemons_from(raw: Option<&str>) -> usize {
    match raw.map(|raw| raw.trim().parse::<usize>()) {
        Some(Ok(n)) if n > 0 => n,
        _ => MAX_LIVE_DAEMONS,
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
    /// The blob schema whose namespace the daemon reads ([`crate::extract::SCHEMA_VER`]). `None` in a
    /// record a release up to 0.33 wrote: `crate::store_gc_namespaces` then reads the namespace
    /// off `version`.
    #[serde(default)]
    pub blob_schema: Option<u16>,
    /// Unix seconds when the daemon acquired the lock.
    pub started_unix: i64,
    /// The daemon's own `crate::boot_identity::process_start`: the process at `pid` is the daemon
    /// exactly when its start is this one. `None` in a record an older release wrote, or where the
    /// platform gives no start; [`crate::process_start::can_have_written`] then falls back to
    /// comparing wall-clock times.
    #[serde(default)]
    pub process_start: Option<u64>,
    /// The boot the daemon ran in (`crate::boot_identity::current`), without which a Linux
    /// `process_start` (ticks since boot) could match a process of a later boot.
    #[serde(default)]
    pub boot: Option<String>,
}

/// The registry root (`<data_home>/daemons/`), holding one subdirectory per generation. Releases
/// up to 0.33 keep their records directly in it; their scan reads only `*.pid` files there, so a
/// generation subdirectory is invisible to them.
fn daemons_root() -> PathBuf {
    daemons_root_of(&DataRoot::current())
}

/// The registry root of the data home `data`.
fn daemons_root_of(data: &DataRoot) -> PathBuf {
    data.path().join(DAEMONS_SUBDIR)
}

/// This generation's registry directory (`<data_home>/daemons/g<N>/`): where this binary's daemons
/// register and the only place its listings look. Keyed on the resolved data home; the `_in`/`_at`
/// helpers take a directory explicitly so unit tests inject a temp dir without mutating the
/// process-global `BASEMIND_DATA_HOME`.
pub fn daemons_dir() -> PathBuf {
    generation_registry_in(&daemons_root())
}

/// This generation's registry directory under an explicit registry root.
fn generation_registry_in(root: &Path) -> PathBuf {
    root.join(format!("g{RENDEZVOUS_GENERATION}"))
}

/// Whether `name` names a generation's directory, in the registry or in a comms directory: `g`
/// followed by decimal digits.
pub fn is_generation_name(name: &str) -> bool {
    name.strip_prefix('g')
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
}

/// The registry pidfile for a daemon dir within a given machine dir: `<machine_dir>/<kind>-<hash>.pid`.
/// The kind prefix scopes the entry to its family, so one family's registration can never clobber
/// another's — and a kind-filtered scan stays correct even for identically-keyed directories.
fn machine_registry_path_in(machine_dir: &Path, kind: DaemonKind, dir: &Path) -> PathBuf {
    machine_dir.join(format!("{}-{}.pid", kind.as_str(), workspace_key(dir)))
}

pub use crate::boot_identity::pid_is_live;

/// Read a daemon record from a pidfile, or `None` when absent/corrupt. A `None` here deregisters the
/// holder (the caller reaps the file), which is why [`DaemonRecord`]'s shape only ever grows in
/// backward-compatible ways.
fn read_record(path: &Path) -> Option<DaemonRecord> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// The live process `<dir>/daemon.pid` names, if any. Only the lock holder writes that file, but
/// it outlives a daemon that was killed, so the process may be an unrelated one that was given
/// the pid since. A caller about to signal it must also find `daemon.lock` held, the process the
/// one the record names ([`crate::process_start::can_have_written`]), and the pid equal to the
/// one the daemon reported, when it reported one.
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

/// `live_daemons` restricted to one family. Powers the per-kind ceiling
/// (`count_live_daemons_of`) and `comms stop --all`, which addresses holders over a
/// kind-specific protocol and must not touch another family's daemons.
pub fn live_daemons_of(kind: DaemonKind) -> Vec<DaemonRecord> {
    live_daemons_of_in(&daemons_dir(), kind)
}

/// `live_daemons_of` over an explicit machine dir. Injected so unit tests use a temp registry.
fn live_daemons_of_in(machine_dir: &Path, kind: DaemonKind) -> Vec<DaemonRecord> {
    let mut live = live_daemons_in(machine_dir);
    live.retain(|record| record.kind == kind);
    live
}

/// `live_daemons` over an explicit machine dir. Injected so unit tests use a temp registry.
fn live_daemons_in(machine_dir: &Path) -> Vec<DaemonRecord> {
    live_daemons_judged_by(machine_dir, crate::boot_identity::pid_state)
}

/// [`live_daemons_in`] with what runs at a pid read by `state`. A record is pruned only when its
/// process is surely gone (`crate::boot_identity::PidState::may_be_live`): one Windows refuses to
/// open may be a daemon of another account or sandbox sharing the data home, and losing its record
/// would hide it from `comms doctor` and `stop --all` and from the ceiling for good. It is listed.
fn live_daemons_judged_by(
    machine_dir: &Path,
    state: impl Fn(u32) -> crate::boot_identity::PidState,
) -> Vec<DaemonRecord> {
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
            Some(record) if state(record.pid).may_be_live() => live.push(record),
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

/// `count_live_daemons_of` over an explicit registry root.
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

/// The registry record of the live daemon running as `pid`, whatever its generation or kind (see
/// [`registered_daemons`]). For naming the process that holds something this daemon wants; a pid
/// alone does not say which basemind version it is. A process that refuses to be opened is not
/// named ([`pid_is_live`]): it is more likely a stranger given a stale record's pid.
pub fn registered_daemon(pid: u32) -> Option<DaemonRecord> {
    registered_daemon_in(&daemons_root(), pid)
}

/// [`registered_daemon`] over an explicit registry root.
fn registered_daemon_in(root: &Path, pid: u32) -> Option<DaemonRecord> {
    registered_daemon_judged_by(root, pid, crate::boot_identity::pid_state)
}

/// [`registered_daemon_in`] with what runs at a pid read by `state`.
fn registered_daemon_judged_by(
    root: &Path,
    pid: u32,
    state: impl Fn(u32) -> crate::boot_identity::PidState,
) -> Option<DaemonRecord> {
    if state(pid) != crate::boot_identity::PidState::Live {
        return None;
    }
    census_of(root, state, crate::process_start::can_have_written)
        .daemons
        .into_iter()
        .map(|daemon| daemon.record)
        .find(|record| record.pid == pid)
}

/// A registered daemon's record and where it was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredDaemon {
    /// The daemon's record.
    pub record: DaemonRecord,
    /// Whether the record sits directly in the registry root, where only releases up to 0.33
    /// record; a later release records under its generation's `g<N>/`.
    pub pre_generation: bool,
}

/// The daemons [`registered_daemons`] lists, and whether it read the whole registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonCensus {
    /// The registered daemons that may be running.
    pub daemons: Vec<RegisteredDaemon>,
    /// Whether every registry directory was listed and every record in it read. A registry, a
    /// generation or a record that does not exist is read as empty: its daemons exited. Any other
    /// read error may leave out a daemon that runs, so a caller keeping what the daemons use keeps
    /// everything when this is false.
    pub complete: bool,
}

/// Every daemon registered under the data home `data` that may be running, of every generation and
/// kind: the records in the registry root first, then those of each `g<N>/`. Read-only. For a
/// caller that keeps what such a daemon uses, so a process that refuses to be opened counts as
/// running (`crate::boot_identity::PidState::may_be_live`), and only a recorded process start
/// that is not the one under the pid passes a record over
/// ([`crate::process_start::may_still_have_written`]): a daemon that was killed left it, and the
/// number now belongs to something else.
pub fn registered_daemons(data: &DataRoot) -> DaemonCensus {
    registered_daemons_in(&daemons_root_of(data))
}

/// [`registered_daemons`] over an explicit registry root.
fn registered_daemons_in(root: &Path) -> DaemonCensus {
    registered_daemons_judged_by(root, crate::boot_identity::pid_state)
}

/// [`registered_daemons_in`] with what runs at a pid read by `state`.
fn registered_daemons_judged_by(root: &Path, state: impl Fn(u32) -> crate::boot_identity::PidState) -> DaemonCensus {
    census_of(root, state, crate::process_start::may_still_have_written)
}

/// The daemons registered under `root` whose pid `state` says may be live and whose record
/// `may_have_written` says the process under that pid can have written.
fn census_of(
    root: &Path,
    state: impl Fn(u32) -> crate::boot_identity::PidState,
    may_have_written: impl Fn(&DaemonRecord) -> bool,
) -> DaemonCensus {
    let (mut records, mut complete) = records_in(root, true);
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries {
            let Ok(entry) = entry else {
                complete = false;
                continue;
            };
            let is_generation = entry.file_name().to_str().is_some_and(is_generation_name) && entry.path().is_dir();
            if is_generation {
                let (generation, listed) = records_in(&entry.path(), false);
                records.extend(generation);
                complete &= listed;
            }
        }
    }
    let daemons = records
        .into_iter()
        .filter(|daemon| state(daemon.record.pid).may_be_live() && may_have_written(&daemon.record))
        .collect();
    DaemonCensus { daemons, complete }
}

/// The `.pid` records directly in `dir`, and whether all of them were read: a directory or record
/// that is gone reads as none, any other error leaves the listing incomplete.
fn records_in(dir: &Path, pre_generation: bool) -> (Vec<RegisteredDaemon>, bool) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => return (Vec::new(), error.kind() == std::io::ErrorKind::NotFound),
    };
    let mut complete = true;
    let mut records = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            complete = false;
            continue;
        };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("pid") {
            continue;
        }
        match std::fs::read(&path) {
            Ok(bytes) => records.extend(
                serde_json::from_slice::<DaemonRecord>(&bytes)
                    .ok()
                    .map(|record| RegisteredDaemon { record, pre_generation }),
            ),
            Err(error) => complete &= error.kind() == std::io::ErrorKind::NotFound,
        }
    }
    (records, complete)
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

    /// [`acquire_at`](Self::acquire_at) that waits, up to `wait`, for a holder that does not
    /// answer to let go, polling every `poll`. A holder that answers (`holder_answers`) is a live
    /// daemon to converge on at once; one that holds the lock without answering is draining (its
    /// endpoint is gone first and its lock last) or still starting. Converging on a draining holder
    /// left no daemon at all once it exited, and the client that spawned this one waited out its
    /// whole spawn timeout.
    pub fn acquire_waiting(
        kind: DaemonKind,
        dir: &Path,
        version: &str,
        machine_dir: &Path,
        (wait, poll): (std::time::Duration, std::time::Duration),
        mut holder_answers: impl FnMut() -> bool,
    ) -> std::io::Result<DaemonLockOutcome> {
        let deadline = std::time::Instant::now() + wait;
        loop {
            let outcome = Self::acquire_at(kind, dir, version, machine_dir)?;
            if matches!(outcome, DaemonLockOutcome::Acquired(_))
                || std::time::Instant::now() >= deadline
                || holder_answers()
            {
                return Ok(outcome);
            }
            std::thread::sleep(poll);
        }
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
            blob_schema: Some(crate::extract::SCHEMA_VER),
            started_unix: now_unix(),
            process_start: crate::boot_identity::process_start(std::process::id()),
            boot: crate::boot_identity::current(),
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
#[path = "daemon_lock_tests.rs"]
mod tests;
