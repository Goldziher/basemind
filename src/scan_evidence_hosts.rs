//! Hosting markers: the evidence a process that hosts workspaces for others (the daemon) leaves
//! for whoever starts after the machine went down under it. Re-exported from
//! [`crate::scan_evidence`].

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::scan_evidence::VERIFY_BLOBS_FILE;
use crate::store_layout::{CACHE_DIR, WORKSPACES_DIR, cache_root};

/// Directory under the data home with one file per process that hosts workspaces, named by pid.
const HOSTING_DIR: &str = "hosting";

/// How far before the boot began a marker that names no boot must have been written to be taken
/// for one of an earlier boot: the boot time is read off the wall clock, which corrections move.
const BOOT_TIME_SLACK: Duration = Duration::from_secs(60);

/// What a hosting marker holds.
#[derive(Debug, Default, Serialize, Deserialize)]
struct HostRecord {
    /// The boot the host was started in ([`crate::boot_identity`]). A pid alone does not name a
    /// process across a restart of the machine: another process gets the number.
    #[serde(default)]
    boot: Option<String>,
    /// The host's [`crate::boot_identity::process_start`]. The pid alive with this start in the
    /// boot named above is the host itself.
    #[serde(default)]
    started: Option<u64>,
}

/// The boot this process runs in, as the judge of markers sees it.
#[derive(Clone, Copy, Debug, Default)]
struct ThisBoot<'a> {
    /// [`crate::boot_identity::current`].
    id: Option<&'a str>,
    /// [`crate::boot_identity::boot_time`].
    began: Option<SystemTime>,
}

/// What became of the host a marker names.
#[derive(Debug, PartialEq, Eq)]
enum HostFate {
    /// Still hosting.
    Live,
    /// Its process ended while the machine stayed up: every blob it wrote reached the OS, and a
    /// blob is renamed into place whole, so nothing of it is torn.
    DiedAlone,
    /// The machine went down under it: blobs it wrote without an fsync may be torn.
    DiedWithTheMachine,
}

/// Judge a marker. `pid` is what runs at the marker's pid, and a process Windows refuses to open
/// may be the host ([`crate::boot_identity::PidState::may_be_live`]): a marker removed while its
/// host runs is not there to be judged when the host dies with the machine. `same_process` says
/// whether the process now at the marker's pid is the host
/// ([`crate::boot_identity::is_recorded_process`], never so across two boot ids), and `same_boot`
/// whether the marker is of this boot; `None` where it cannot be told. The host's own process still
/// running is `Live` whatever the boot times say: a boot time that moved is not a restart. With the
/// boot unknown and the host gone, the worse case is read.
fn host_fate(pid: crate::boot_identity::PidState, same_process: Option<bool>, same_boot: Option<bool>) -> HostFate {
    let pid_live = pid.may_be_live();
    if pid_live && same_process == Some(true) {
        return HostFate::Live;
    }
    let host_alive = pid_live && same_process.is_none();
    match same_boot {
        Some(false) => HostFate::DiedWithTheMachine,
        _ if host_alive => HostFate::Live,
        Some(true) => HostFate::DiedAlone,
        None => HostFate::DiedWithTheMachine,
    }
}

/// Whether the marker at `path` holding `record` is of this boot. A marker that names no boot
/// comparable with this one is of an earlier boot when it was written well before this one began.
fn marker_of_this_boot(path: &Path, record: &HostRecord, this_boot: ThisBoot<'_>) -> Option<bool> {
    let compared = record
        .boot
        .as_deref()
        .zip(this_boot.id)
        .and_then(|(then, now)| crate::boot_identity::same_boot(then, now));
    compared.or_else(|| {
        let written = std::fs::metadata(path).and_then(|meta| meta.modified()).ok()?;
        (written + BOOT_TIME_SLACK < this_boot.began?).then_some(false)
    })
}

/// Remove the markers of hosts that are gone. When the machine went down under one, ask every
/// workspace under `workspaces_dir` to verify its blobs: the marker does not say which workspaces
/// its host served, and the request must not depend on which process happens to open one next.
/// A marker under this process's own pid is judged like any other: left from an earlier boot it
/// is a dead host's, and from this boot it is this process's own. Returns whether it asked.
#[cfg(any(unix, windows))]
fn settle_dead_hosts_in(hosting_dir: &Path, workspaces_dir: &Path, this_boot: ThisBoot<'_>) -> bool {
    settle_dead_hosts_with(hosting_dir, workspaces_dir, this_boot, |flag| {
        std::fs::write(flag, b"").is_ok()
    })
}

/// [`settle_dead_hosts_in`] writing each verification request with `request`, which says whether
/// it was written. Every marker is judged and every request written before any marker goes: a
/// process killed in between, or a request that could not be written, leaves the markers for the
/// next one to judge again.
#[cfg(any(unix, windows))]
fn settle_dead_hosts_with(
    hosting_dir: &Path,
    workspaces_dir: &Path,
    this_boot: ThisBoot<'_>,
    mut request: impl FnMut(&Path) -> bool,
) -> bool {
    let mut dead = Vec::new();
    let mut machine_went_down = false;
    for entry in std::fs::read_dir(hosting_dir).into_iter().flatten().flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) else {
            continue;
        };
        let path = entry.path();
        let record: HostRecord = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let same_process =
            crate::boot_identity::is_recorded_process(pid, record.started, record.boot.as_deref(), this_boot.id);
        let fate = host_fate(
            crate::boot_identity::pid_state(pid),
            same_process,
            marker_of_this_boot(&path, &record, this_boot),
        );
        if fate == HostFate::Live {
            continue;
        }
        machine_went_down |= fate == HostFate::DiedWithTheMachine;
        dead.push(path);
    }
    let mut asked_every_workspace = true;
    if machine_went_down {
        for workspace in std::fs::read_dir(workspaces_dir).into_iter().flatten().flatten() {
            if workspace.path().is_dir() {
                asked_every_workspace &= request(&workspace.path().join(VERIFY_BLOBS_FILE));
            }
        }
    }
    if !asked_every_workspace {
        tracing::warn!("a request to verify blobs could not be written; the dead hosts' markers are kept");
        return machine_went_down;
    }
    for marker in dead {
        let _ = std::fs::remove_file(marker);
    }
    machine_went_down
}

/// [`settle_dead_hosts_in`] against the data home, once per process: a host that dies while this
/// process lives dies in this boot, which asks for nothing. Run by every process about to scan, so
/// a `basemind scan` after a power loss verifies without waiting for a daemon to start.
pub fn settle_dead_hosts() {
    #[cfg(any(unix, windows))]
    {
        // A unit test that set no data home would be reading the developer's own cache. ~keep
        #[cfg(test)]
        if std::env::var_os(crate::store_layout::DATA_HOME_ENV).is_none() {
            return;
        }
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let root = cache_root();
            let id = crate::boot_identity::current();
            settle_dead_hosts_in(
                &root.join(HOSTING_DIR),
                &root.join(CACHE_DIR).join(WORKSPACES_DIR),
                ThisBoot {
                    id: id.as_deref(),
                    began: crate::boot_identity::boot_time(),
                },
            );
        });
    }
}

/// The claim of a process that hosts workspaces for others (the daemon): written when it starts
/// hosting, removed when it stops cleanly. A scan's breadcrumb covers only the scan in flight; this
/// covers everything else a host does between scans, and the scan that finished seconds before the
/// power went. A marker left by a host of an earlier boot means blobs written since may be torn, in
/// any workspace of the shared store.
#[derive(Debug)]
pub struct HostingMarker {
    path: PathBuf,
}

impl HostingMarker {
    /// Begin hosting under the data home. `None` when the marker could not be written: evidence is
    /// never a precondition for work.
    #[cfg(any(unix, windows))]
    pub fn begin() -> Option<Self> {
        settle_dead_hosts();
        Self::begin_in(
            &cache_root().join(HOSTING_DIR),
            crate::boot_identity::current().as_deref(),
        )
    }

    /// Write this process's marker into `dir`, whole or not at all: another host reading a
    /// half-written one would take it for the marker of a host from an unknown boot.
    #[cfg(any(unix, windows))]
    fn begin_in(dir: &Path, boot: Option<&str>) -> Option<Self> {
        std::fs::create_dir_all(dir).ok()?;
        let record = HostRecord {
            boot: boot.map(str::to_string),
            started: crate::boot_identity::process_start(std::process::id()),
        };
        let bytes = serde_json::to_vec(&record).ok()?;
        let path = dir.join(std::process::id().to_string());
        let temp = dir.join(format!(".{}.tmp", std::process::id()));
        std::fs::write(&temp, bytes).ok()?;
        if std::fs::rename(&temp, &path).is_err() {
            let _ = std::fs::remove_file(&temp);
            return None;
        }
        Some(Self { path })
    }
}

impl Drop for HostingMarker {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(all(test, any(unix, windows)))]
#[path = "scan_evidence_hosts_tests.rs"]
mod tests;
