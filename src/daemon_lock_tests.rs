//! Unit tests for the daemon lock and the machine registry.

use super::*;

#[test]
fn self_pid_is_live_and_a_bogus_pid_is_not() {
    assert!(pid_is_live(std::process::id()), "our own pid is live");
    // A pid well above any real one on a fresh machine reads as dead.
    assert!(!pid_is_live(0x7FFF_FFFE), "an absent pid is not live");
}

/// The pid of a process that has exited is not live, even while a handle to it is still held:
/// `child` keeps one open on Windows until it is dropped.
#[test]
fn an_exited_process_is_not_live_while_its_handle_is_held() {
    let mut child = std::process::Command::new(std::env::current_exe().expect("this test binary"))
        .arg("--list")
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn a child");
    assert!(child.wait().expect("wait for the child").success());
    assert!(!pid_is_live(child.id()), "an exited process is not live");
}

/// A process that may not be opened reads as not live, but as possibly live to a caller that would
/// delete its record: the System process, pid 4, is refused to an unelevated account. The mapping
/// itself is tested on every platform (`windows_answers_map_to_a_pid_state`).
#[cfg(windows)]
#[test]
#[ignore = "an elevated account, as on GitHub's Windows runners, may open pid 4; run it unelevated"]
fn a_process_that_may_not_be_opened_is_not_live() {
    use crate::boot_identity::{PidState, pid_state};
    const SYSTEM_PID: u32 = 4;
    assert_eq!(pid_state(SYSTEM_PID), PidState::Unopenable, "run unelevated");
    assert!(!pid_is_live(SYSTEM_PID));
    assert!(pid_state(SYSTEM_PID).may_be_live());
}

#[test]
fn a_live_daemon_is_found_by_pid_in_any_generation_and_a_dead_one_is_not() {
    let root = tempfile::tempdir().expect("tempdir");
    let record = |pid: u32, version: &str, started_unix: i64| {
        serde_json::json!({
            "pid": pid, "kind": "comms", "dir": "/tmp/comms", "version": version, "started_unix": started_unix,
        })
        .to_string()
    };
    let me = std::process::id();
    assert!(
        registered_daemon_in(root.path(), me).is_none(),
        "an empty registry names nobody"
    );

    std::fs::write(
        root.path().join("comms-dead.pid"),
        record(0x7FFF_FFFE, "0.33.0", now_unix()),
    )
    .expect("write");
    assert!(
        registered_daemon_in(root.path(), 0x7FFF_FFFE).is_none(),
        "a dead pid is nobody"
    );

    let other = root.path().join("g7");
    std::fs::create_dir_all(&other).expect("mkdir generation");
    std::fs::write(other.join("comms-x.pid"), record(me, "0.99.0", now_unix())).expect("write");
    let found = registered_daemon_in(root.path(), me).expect("a record in another generation");
    assert_eq!(found.version, "0.99.0");

    std::fs::write(root.path().join("comms-old.pid"), record(me, "0.33.0", now_unix())).expect("write");
    let found = registered_daemon_in(root.path(), me).expect("a record in the registry root");
    assert_eq!(found.version, "0.33.0", "the pre-generation layout is read too");
}

/// A 0.33 daemon was killed and left its record; its pid now belongs to a newer daemon. The
/// stale record is older than that process, so it does not name it.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn a_record_older_than_the_process_under_its_pid_does_not_name_that_process() {
    let root = tempfile::tempdir().expect("tempdir");
    let record = |version: &str, started_unix: i64| {
        serde_json::json!({
            "pid": std::process::id(), "kind": "comms", "dir": "/tmp/comms", "version": version,
            "started_unix": started_unix,
        })
        .to_string()
    };
    std::fs::write(root.path().join("comms-stale.pid"), record("0.33.0", 1)).expect("write");
    assert!(registered_daemon_in(root.path(), std::process::id()).is_none());

    let current = root.path().join("g7");
    std::fs::create_dir_all(&current).expect("mkdir generation");
    std::fs::write(current.join("comms-x.pid"), record("0.99.0", now_unix())).expect("write");
    let found = registered_daemon_in(root.path(), std::process::id()).expect("the record it wrote");
    assert_eq!(found.version, "0.99.0");
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
    assert_eq!(
        (record.process_start, record.boot),
        (None, None),
        "no start recorded: the wall-clock rule applies"
    );
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
    match DaemonLock::acquire_at(DaemonKind::Comms, comms.path(), "9.9.9", machine.path()).expect("second acquire") {
        DaemonLockOutcome::AlreadyHeld(Some(record)) => {
            assert_eq!(record.pid, std::process::id(), "the holder record names us");
            assert_eq!(
                record.process_start,
                crate::boot_identity::process_start(std::process::id()),
                "and our process start"
            );
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
        process_start: None,
        boot: None,
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

/// A record whose process Windows refuses to open may name a live daemon of another account or
/// sandbox sharing the data home: it is listed and its pidfile kept, never pruned as dead.
#[test]
fn a_record_whose_process_refuses_to_be_opened_is_kept() {
    use crate::boot_identity::{PidState, windows_state};
    const ERROR_ACCESS_DENIED: i32 = 5;
    let machine = tempfile::tempdir().expect("machine tempdir");
    let record = DaemonRecord {
        pid: 0x7FFF_FFFE,
        kind: DaemonKind::Comms,
        dir: PathBuf::from("/nonexistent"),
        version: "9.9.9".to_string(),
        started_unix: now_unix(),
        process_start: None,
        boot: None,
    };
    let path = machine.path().join("unopenable.pid");
    write_record(&path, &record);

    let unopenable = |_pid: u32| windows_state(Err(Some(ERROR_ACCESS_DENIED)));
    let listed = live_daemons_judged_by(machine.path(), unopenable);
    assert_eq!(listed.iter().map(|r| r.pid).collect::<Vec<_>>(), [record.pid]);
    assert!(path.exists(), "the pidfile is kept");

    assert!(live_daemons_judged_by(machine.path(), |_pid| PidState::Gone).is_empty());
    assert!(!path.exists(), "a gone process's pidfile is pruned");
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

/// A registry record as a release up to 0.33 writes it. Serialized, not formatted: a Windows `dir`
/// holds backslashes, which JSON must escape.
fn comms_record(pid: u32, dir: &str) -> String {
    serde_json::json!({"pid": pid, "kind": "comms", "dir": dir, "version": "0.33.0", "started_unix": 1}).to_string()
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
        let held = DaemonLock::acquire_at(DaemonKind::Comms, comms.path(), "9.9.9", machine.path()).expect("acquire");
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
