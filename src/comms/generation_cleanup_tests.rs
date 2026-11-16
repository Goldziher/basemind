//! Unit tests for the rendezvous sweep.

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

/// Entries left directly in `base`, sorted.
fn left_in(base: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(base)
        .expect("read base")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Whether this process can open `path` despite a mode that denies it, as root can: the tests
/// that rely on a permission error have nothing to observe then.
#[cfg(unix)]
fn permissions_are_ignored(path: &Path) -> bool {
    File::options().read(true).write(true).open(path).is_ok()
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

#[test]
fn a_generation_directory_without_a_daemon_lock_is_not_a_rendezvous() {
    let base = tempfile::tempdir().expect("tempdir");
    let stray = base.path().join(other());
    std::fs::create_dir_all(stray.join("store.fjall")).expect("mkdir");
    std::fs::write(stray.join("store.fjall/data"), b"12345").expect("write");
    std::fs::write(stray.join("notes.txt"), b"mine").expect("write");

    let findings = sweep(base.path(), DEFAULT_MAX_AGE, false, later());

    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0].verdict, Verdict::Skipped);
    assert!(stray.join("store.fjall/data").exists(), "nothing here was locked by us");
    assert!(stray.join("notes.txt").exists());
}

/// A later release may lock its generation some other way; a directory this binary cannot lock
/// by `daemon.lock` is not this binary's to remove.
#[test]
fn a_generation_locked_by_another_scheme_is_left_alone() {
    let base = tempfile::tempdir().expect("tempdir");
    let future = base.path().join(format!("g{}", RENDEZVOUS_GENERATION + 7));
    std::fs::create_dir_all(future.join("store")).expect("mkdir");
    for file in ["rendezvous.lockdb", "daemon.pid", "store/data"] {
        std::fs::write(future.join(file), b"12345").expect("write");
    }

    let findings = sweep(base.path(), Duration::ZERO, false, later());

    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0].verdict, Verdict::Skipped);
    assert!(future.join("store/data").exists());
    assert!(future.join("rendezvous.lockdb").exists());
}

#[test]
fn an_empty_generation_directory_is_removed_without_a_lock() {
    let base = tempfile::tempdir().expect("tempdir");
    let empty = base.path().join(other());
    std::fs::create_dir_all(&empty).expect("mkdir");

    let findings = sweep(base.path(), DEFAULT_MAX_AGE, false, later());

    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0].verdict, Verdict::Removed);
    assert!(!empty.exists());
}

/// `BASEMIND_COMMS_DIR` may name a directory basemind shares with other things. Files there that
/// merely carry a pre-generation name are no rendezvous unless a `daemon.lock` sits beside them.
#[test]
fn pre_generation_names_without_a_daemon_lock_are_left_alone() {
    let base = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(base.path().join("store.fjall")).expect("mkdir");
    for file in ["store.fjall/data", "daemon.log", "http.addr"] {
        std::fs::write(base.path().join(file), b"12345").expect("write");
    }

    let findings = sweep(base.path(), Duration::ZERO, false, later());

    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0].rendezvous, Rendezvous::PreGeneration);
    assert_eq!(findings[0].verdict, Verdict::Skipped);
    assert_eq!(left_in(base.path()), ["daemon.log", "http.addr", "store.fjall"]);
    assert!(base.path().join("store.fjall/data").exists());
}

/// Another user's daemon lock cannot be opened, let alone taken; whether it is held is unknown.
#[cfg(unix)]
#[test]
fn a_lock_that_cannot_be_opened_counts_as_held() {
    let base = tempfile::tempdir().expect("tempdir");
    let foreign = generation(base.path(), &other());
    pre_generation(base.path());
    let locks = [foreign.join("daemon.lock"), base.path().join("daemon.lock")];
    for lock in &locks {
        set_mode(lock, 0o000);
    }
    if permissions_are_ignored(&locks[0]) {
        return;
    }

    let findings = sweep(base.path(), DEFAULT_MAX_AGE, false, later());

    for lock in locks.iter().filter(|lock| lock.exists()) {
        set_mode(lock, 0o600);
    }
    assert_eq!(findings.len(), 2, "{findings:?}");
    assert!(findings.iter().all(|f| f.verdict == Verdict::InUse), "{findings:?}");
    assert!(foreign.join("store.fjall/data").exists());
    assert!(base.path().join("store.fjall/data").exists());
    assert!(base.path().join("comms.sock").exists());
}

/// A pid no process has.
const DEAD_PID: u32 = 0x7FFF_FFFE;

fn removing(name: &str, pid: u32) -> String {
    format!("{name}{REMOVING_INFIX}{pid}")
}

/// Put a directory inside `dir` that cannot be emptied, so deleting `dir` fails part-way.
/// Returns it, for the test to make deletable again.
#[cfg(unix)]
fn undeletable_inside(dir: &Path) -> PathBuf {
    let stuck = dir.join("stuck");
    std::fs::create_dir_all(&stuck).expect("mkdir");
    std::fs::write(stuck.join("file"), b"12345").expect("write");
    set_mode(&stuck, 0o500);
    stuck
}

/// A daemon of that generation starting while the sweep deletes must find its directory either
/// whole or absent. So the name has to be free from the first step, even when deleting then fails.
#[cfg(unix)]
#[test]
fn a_generation_leaves_its_name_in_one_step_and_a_failed_deletion_is_finished_by_the_next_sweep() {
    let base = tempfile::tempdir().expect("tempdir");
    let old = generation(base.path(), &other());
    let stuck = undeletable_inside(&old.join("store.fjall"));
    if std::fs::write(stuck.join("probe"), b"").is_ok() {
        return;
    }

    let findings = sweep(base.path(), DEFAULT_MAX_AGE, false, later());

    let aside = base.path().join(removing(&other(), std::process::id()));
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0].verdict, Verdict::Failed);
    assert!(
        !old.exists(),
        "the generation's name is free although its files are not all gone"
    );
    assert!(
        aside.join("store.fjall/stuck/file").exists(),
        "what is left sits under the aside name"
    );

    set_mode(&aside.join("store.fjall/stuck"), 0o700);
    let findings = sweep(base.path(), DEFAULT_MAX_AGE, false, SystemTime::now());

    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(
        findings[0].rendezvous,
        Rendezvous::Leftover(removing(&other(), std::process::id()))
    );
    assert_eq!(findings[0].verdict, Verdict::Removed, "whatever its age");
    assert_eq!(left_in(base.path()), Vec::<String>::new());
}

#[cfg(unix)]
#[test]
fn the_pre_generation_store_is_moved_aside_before_anything_is_deleted() {
    let base = tempfile::tempdir().expect("tempdir");
    pre_generation(base.path());
    let stuck = undeletable_inside(&base.path().join("store.fjall"));
    if std::fs::write(stuck.join("probe"), b"").is_ok() {
        return;
    }

    let findings = sweep(base.path(), DEFAULT_MAX_AGE, false, later());

    let aside = removing("store.fjall", std::process::id());
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0].verdict, Verdict::Failed);
    assert_eq!(
        left_in(base.path()),
        std::slice::from_ref(&aside),
        "the store is not left half-deleted under its own name, and the lock is gone with the rest"
    );
    set_mode(&base.path().join(aside).join("stuck"), 0o700);
}

#[test]
fn what_a_dead_sweep_left_aside_is_removed_and_a_running_sweeps_is_not() {
    let base = tempfile::tempdir().expect("tempdir");
    let dead_generation = removing(&other(), DEAD_PID);
    let dead_store = removing("store.fjall", DEAD_PID);
    // A process other than this one that is certainly running: the test runner's parent, and on
    // Windows, which names no parent, the System process.
    #[cfg(unix)]
    let running = removing(&other(), std::os::unix::process::parent_id());
    #[cfg(windows)]
    let running = removing(&other(), 4);
    let unrelated = removing("notes", DEAD_PID);
    for name in [&dead_generation, &dead_store, &running, &unrelated] {
        std::fs::create_dir_all(base.path().join(name)).expect("mkdir");
        std::fs::write(base.path().join(name).join("data"), b"12345").expect("write");
    }

    let dry = sweep(base.path(), DEFAULT_MAX_AGE, true, SystemTime::now());
    assert!(
        base.path().join(&dead_generation).join("data").exists(),
        "a dry run deletes nothing"
    );
    let findings = sweep(base.path(), DEFAULT_MAX_AGE, false, SystemTime::now());

    assert_eq!(findings, dry, "a dry run reports what a real run does");
    assert_eq!(findings.len(), 2, "{findings:?}");
    assert!(
        base.path().join(&running).join("data").exists(),
        "another sweep is deleting that one"
    );
    assert_eq!(findings[0].rendezvous, Rendezvous::Leftover(dead_generation.clone()));
    assert_eq!(findings[0].bytes, 5);
    assert!(findings.iter().all(|f| f.verdict == Verdict::Removed), "{findings:?}");
    assert!(!base.path().join(&dead_generation).exists());
    assert!(!base.path().join(&dead_store).exists());
    assert!(
        base.path().join(&unrelated).join("data").exists(),
        "only names a sweep makes"
    );
}
