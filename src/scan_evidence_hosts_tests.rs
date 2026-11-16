//! Tests of hosting markers and how a later process judges them.

use super::*;
#[cfg(unix)]
use crate::scan_evidence::blobs_need_verifying;

#[cfg(unix)]
const THIS_BOOT_ID: &str = "id:this";
#[cfg(unix)]
const EARLIER_BOOT_ID: &str = "id:earlier";

#[cfg(unix)]
fn this_boot() -> ThisBoot<'static> {
    ThisBoot {
        id: Some(THIS_BOOT_ID),
        began: None,
    }
}

/// Plant the marker of a host with `pid`, started in `boot` at `started`.
#[cfg(unix)]
fn plant_host(hosting: &Path, pid: u32, boot: Option<&str>, started: Option<u64>) -> PathBuf {
    std::fs::create_dir_all(hosting).expect("hosting dir");
    let path = hosting.join(pid.to_string());
    let record = HostRecord {
        boot: boot.map(str::to_string),
        started,
    };
    std::fs::write(&path, serde_json::to_vec(&record).expect("encode")).expect("plant");
    path
}

#[cfg(unix)]
fn two_workspaces(root: &Path) -> (PathBuf, [PathBuf; 2]) {
    let workspaces = root.join("workspaces");
    let pair = [workspaces.join("a"), workspaces.join("b")];
    for dir in &pair {
        std::fs::create_dir_all(dir).expect("workspace");
    }
    (workspaces, pair)
}

/// A pid no process has.
#[cfg(unix)]
const DEAD_PID: u32 = u32::MAX - 11;
/// A pid some other process always has.
#[cfg(unix)]
const LIVE_PID: u32 = 1;

#[cfg(unix)]
#[test]
fn a_clean_stop_retracts_the_marker_and_the_marker_names_its_boot_and_start() {
    let dir = tempfile::tempdir().expect("tempdir");
    let marker = HostingMarker::begin_in(dir.path(), Some(THIS_BOOT_ID)).expect("marker");
    let own = dir.path().join(std::process::id().to_string());
    let record: HostRecord = serde_json::from_slice(&std::fs::read(&own).expect("read")).expect("decode");
    assert_eq!(record.boot.as_deref(), Some(THIS_BOOT_ID));
    assert_eq!(record.started, crate::boot_identity::process_start(std::process::id()));
    assert_eq!(
        std::fs::read_dir(dir.path()).expect("list").count(),
        1,
        "no temp file left"
    );
    drop(marker);
    assert!(!own.exists());
}

/// The power went with a host at some pid, and after the restart another process has that
/// number. The pid being alive says nothing: the marker is of an earlier boot.
#[cfg(unix)]
#[test]
fn a_marker_of_an_earlier_boot_is_a_dead_host_even_when_its_pid_is_alive() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hosting = dir.path().join("hosting");
    let (workspaces, pair) = two_workspaces(dir.path());
    let stale = plant_host(&hosting, LIVE_PID, Some(EARLIER_BOOT_ID), None);

    assert!(settle_dead_hosts_in(&hosting, &workspaces, this_boot()));

    assert!(!stale.exists(), "reported once");
    assert!(
        pair.iter().all(|workspace| blobs_need_verifying(workspace)),
        "every workspace the dead host may have served verifies, not only the next one opened"
    );
    assert!(!settle_dead_hosts_in(&hosting, &workspaces, this_boot()));

    // Verified once, not again on every later look at the markers.
    crate::scan_evidence::blobs_verified(&pair[0]);
    assert!(!settle_dead_hosts_in(&hosting, &workspaces, this_boot()));
    assert!(!blobs_need_verifying(&pair[0]));
}

/// A boot reading that changed while the host kept running is not a restart: the process at the
/// pid is the one that wrote the marker, so the host is live and nothing is verified.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_live_host_whose_start_matches_is_live_whatever_its_boot_reads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hosting = dir.path().join("hosting");
    let (workspaces, pair) = two_workspaces(dir.path());
    let pid = std::process::id();
    let started = crate::boot_identity::process_start(pid).expect("own start");
    let live = plant_host(&hosting, pid, Some("t:1000"), Some(started));

    let this_boot = ThisBoot {
        id: Some("t:5000"),
        began: None,
    };
    assert!(!settle_dead_hosts_in(&hosting, &workspaces, this_boot));

    assert!(live.exists(), "the live host keeps its marker");
    assert!(pair.iter().all(|workspace| !blobs_need_verifying(workspace)));
}

/// A live pid whose process started after the host did is another process given the number.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_live_pid_with_another_start_is_a_dead_host() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hosting = dir.path().join("hosting");
    let (workspaces, pair) = two_workspaces(dir.path());
    let pid = std::process::id();
    let started = crate::boot_identity::process_start(pid).expect("own start");
    let reused = plant_host(&hosting, pid, Some(THIS_BOOT_ID), Some(started.wrapping_add(1)));

    assert!(!settle_dead_hosts_in(&hosting, &workspaces, this_boot()));

    assert!(!reused.exists(), "a host that died in this boot is noticed");
    assert!(pair.iter().all(|workspace| !blobs_need_verifying(workspace)));
}

/// A process killed after it asked for verification but before it removed the markers leaves them
/// for the next process, which asks again; killed before asking, the evidence must not be gone.
#[cfg(unix)]
#[test]
fn markers_outlive_a_process_killed_before_it_asked_for_verification() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hosting = dir.path().join("hosting");
    let (workspaces, pair) = two_workspaces(dir.path());
    let stale = plant_host(&hosting, DEAD_PID, Some(EARLIER_BOOT_ID), None);

    let killed = std::panic::catch_unwind(|| {
        settle_dead_hosts_with(&hosting, &workspaces, this_boot(), |_| panic!("killed"));
    });

    assert!(killed.is_err());
    assert!(stale.exists(), "the marker is the only evidence left");
    assert!(settle_dead_hosts_in(&hosting, &workspaces, this_boot()));
    assert!(pair.iter().all(|workspace| blobs_need_verifying(workspace)));
    assert!(!stale.exists());
}

/// A marker that names no boot (written before markers did) under a pid no process has is the
/// marker of a host that died: it is noticed, once, and a clean stop leaves nothing to notice.
#[cfg(unix)]
#[test]
fn a_host_that_died_is_noticed_once_and_a_clean_stop_is_not() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hosting = dir.path().join("hosting");
    let (workspaces, pair) = two_workspaces(dir.path());
    drop(HostingMarker::begin_in(&hosting, Some(THIS_BOOT_ID)).expect("marker"));
    assert!(
        !settle_dead_hosts_in(&hosting, &workspaces, this_boot()),
        "a clean stop retracted the marker"
    );

    let dead = hosting.join(DEAD_PID.to_string());
    std::fs::write(&dead, b"").expect("plant");
    assert!(settle_dead_hosts_in(&hosting, &workspaces, this_boot()));
    assert!(!dead.exists(), "reported once");
    assert!(pair.iter().all(|workspace| blobs_need_verifying(workspace)));
    assert!(!settle_dead_hosts_in(&hosting, &workspaces, this_boot()));
}

/// A marker that names no boot, under a pid that a process of this boot now holds, was written
/// before this boot began: its host went down with the machine.
#[cfg(unix)]
#[test]
fn a_marker_naming_no_boot_written_before_this_boot_is_a_dead_host_though_its_pid_lives() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hosting = dir.path().join("hosting");
    let (workspaces, pair) = two_workspaces(dir.path());
    let legacy = hosting.join(LIVE_PID.to_string());
    std::fs::create_dir_all(&hosting).expect("hosting dir");
    std::fs::write(&legacy, b"{}").expect("plant");
    let now = SystemTime::now();
    std::fs::File::options()
        .write(true)
        .open(&legacy)
        .and_then(|file| file.set_modified(now - Duration::from_secs(3600)))
        .expect("age the marker");

    let written_this_boot = ThisBoot {
        id: Some(THIS_BOOT_ID),
        began: Some(now - Duration::from_secs(2 * 3600)),
    };
    assert!(!settle_dead_hosts_in(&hosting, &workspaces, written_this_boot));
    assert!(legacy.exists(), "written in this boot under a live pid: kept");

    let written_before = ThisBoot {
        id: Some(THIS_BOOT_ID),
        began: Some(now - Duration::from_secs(60)),
    };
    assert!(settle_dead_hosts_in(&hosting, &workspaces, written_before));
    assert!(!legacy.exists());
    assert!(pair.iter().all(|workspace| blobs_need_verifying(workspace)));
}

/// A host killed while the machine stayed up cannot have torn a blob: its marker goes and
/// nothing is verified.
#[cfg(unix)]
#[test]
fn a_host_that_died_in_this_boot_asks_for_no_verification() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hosting = dir.path().join("hosting");
    let (workspaces, pair) = two_workspaces(dir.path());
    let killed = plant_host(&hosting, DEAD_PID, Some(THIS_BOOT_ID), None);

    assert!(!settle_dead_hosts_in(&hosting, &workspaces, this_boot()));

    assert!(!killed.exists(), "the marker of a host that is gone is removed");
    assert!(pair.iter().all(|workspace| !blobs_need_verifying(workspace)));
}

#[cfg(unix)]
#[test]
fn a_live_host_of_this_boot_keeps_its_marker() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hosting = dir.path().join("hosting");
    let (workspaces, pair) = two_workspaces(dir.path());
    let live = plant_host(&hosting, LIVE_PID, Some(THIS_BOOT_ID), None);

    assert!(!settle_dead_hosts_in(&hosting, &workspaces, this_boot()));

    assert!(live.exists());
    assert!(pair.iter().all(|workspace| !blobs_need_verifying(workspace)));
}

/// The marker of an earlier boot can carry this very pid: the number was handed out again, to
/// this process. It is still a dead host's.
#[cfg(unix)]
#[test]
fn a_marker_of_an_earlier_boot_under_this_process_own_pid_is_a_dead_host() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hosting = dir.path().join("hosting");
    let (workspaces, pair) = two_workspaces(dir.path());
    plant_host(&hosting, std::process::id(), Some(EARLIER_BOOT_ID), None);

    assert!(settle_dead_hosts_in(&hosting, &workspaces, this_boot()));
    assert!(pair.iter().all(|workspace| blobs_need_verifying(workspace)));

    let own = plant_host(&hosting, std::process::id(), Some(THIS_BOOT_ID), None);
    assert!(!settle_dead_hosts_in(&hosting, &workspaces, this_boot()));
    assert!(own.exists(), "this process's own marker of this boot is left alone");
}

#[test]
fn a_host_is_judged_by_its_process_first_and_its_boot_second() {
    use HostFate::{DiedAlone, DiedWithTheMachine, Live};
    assert_eq!(
        host_fate(true, Some(true), Some(false)),
        Live,
        "the host itself still runs"
    );
    assert_eq!(
        host_fate(true, Some(false), Some(true)),
        DiedAlone,
        "the pid went to another"
    );
    assert_eq!(host_fate(true, Some(false), Some(false)), DiedWithTheMachine);
    assert_eq!(host_fate(true, Some(false), None), DiedWithTheMachine);
    assert_eq!(host_fate(true, None, Some(false)), DiedWithTheMachine);
    assert_eq!(host_fate(true, None, Some(true)), Live);
    assert_eq!(host_fate(true, None, None), Live, "a live pid is all there is");
    assert_eq!(host_fate(false, None, Some(true)), DiedAlone);
    assert_eq!(
        host_fate(false, None, None),
        DiedWithTheMachine,
        "with the boot unknown a dead pid is the worse case"
    );
}
