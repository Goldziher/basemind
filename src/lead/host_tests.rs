//! The lead host in this process, against a `/bin/sh` stand-in for Claude Code that reads its
//! input until it is stopped. No real harness is started; the policy file and the audit directory
//! are temp paths, never the user's.

use std::os::unix::fs::PermissionsExt as _;

use super::*;

struct Fixture {
    _dir: tempfile::TempDir,
    base: PathBuf,
    lead_root: PathBuf,
    worktrees: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        crate::store::init_isolated_cache();
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        for sub in ["lead", "worktrees/one", "worktrees/two", "config"] {
            std::fs::create_dir_all(base.join(sub)).unwrap();
        }
        std::fs::set_permissions(base.join("config"), std::fs::Permissions::from_mode(0o700)).unwrap();
        let stand_in = base.join("claude-stand-in");
        std::fs::write(&stand_in, "#!/bin/sh\ncat > /dev/null\n").unwrap();
        std::fs::set_permissions(&stand_in, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            lead_root: base.join("lead"),
            worktrees: base.join("worktrees"),
            _dir: dir,
            base,
        }
    }

    fn policy(&self) -> PathBuf {
        self.policy_at(&self.worktrees, "agents.toml")
    }

    /// A policy file `name` whose worktrees root is `root`.
    fn policy_at(&self, root: &Path, name: &str) -> PathBuf {
        let path = self.base.join("config").join(name);
        std::fs::write(
            &path,
            format!(
                "[agents.launch]\nenabled = true\nworktrees_root = \"{}\"\nwall_time_secs = 60\n\
                 [agents.launch.harness.claude]\nexecutable = \"{}\"\nstop_grace_secs = 1\n",
                root.display(),
                self.base.join("claude-stand-in").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    fn start(&self, policy_path: PathBuf, audit_dir: PathBuf) -> Result<LeadHost, LeadOff> {
        LeadHost::start(
            &self.lead_root,
            Principal::parse("lead-agent").unwrap(),
            None,
            Setup {
                policy_path,
                audit_dir,
                worktree_locks: self.base.join("locks"),
                registry: Registry::with_defaults(),
            },
        )
    }
}

fn body(result: &Value) -> Value {
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[test]
fn without_a_policy_file_the_host_stays_off_and_says_where_the_file_belongs() {
    let fixture = Fixture::new();
    let missing = fixture.base.join("config/agents.toml");
    let off = fixture
        .start(missing.clone(), fixture.base.join("audit"))
        .err()
        .unwrap();
    assert!(off.0.contains("agent launching is off"), "{off}");
    assert!(off.0.contains(&missing.display().to_string()), "{off}");
}

/// Review 9 condition 7: no audit log, no launching.
#[test]
fn an_audit_log_that_cannot_be_opened_keeps_launching_off() {
    let fixture = Fixture::new();
    let blocked = fixture.base.join("audit-is-a-file");
    std::fs::write(&blocked, "").unwrap();
    let off = fixture.start(fixture.policy(), blocked).err().unwrap();
    assert!(off.0.contains("audit log"), "{off}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_lead_launches_into_its_own_worktree_and_stops_what_it_launched() {
    let fixture = Fixture::new();
    let host = Arc::new(fixture.start(fixture.policy(), fixture.base.join("audit")).unwrap());
    let call = |name: &'static str, arguments: Value| Arc::clone(&host).call(name, arguments);

    for (arguments, code) in [
        (
            json!({ "harness": "gemini", "brief": "b", "working_dir": "one" }),
            "flavor_not_allowed",
        ),
        (
            json!({ "harness": "claude", "brief": "b", "working_dir": "" }),
            "working_dir_invalid",
        ),
        (
            json!({ "harness": "claude", "brief": "b", "working_dir": "." }),
            "working_dir_invalid",
        ),
        (
            json!({ "harness": "claude", "brief": "b", "working_dir": "../lead" }),
            "working_dir_invalid",
        ),
        (
            json!({ "harness": "claude", "brief": "b", "working_dir": "/tmp" }),
            "working_dir_invalid",
        ),
        (
            json!({ "harness": "claude", "brief": "b", "working_dir": "missing" }),
            "working_dir_invalid",
        ),
    ] {
        let refused = call(AGENT_LAUNCH, arguments.clone()).await;
        assert_eq!(refused["isError"], true, "{arguments}");
        assert_eq!(body(&refused)["error"], code, "{arguments}");
    }
    let unknown = call(
        AGENT_LAUNCH,
        json!({ "harness": "claude", "brief": "b", "working_dir": "one", "executable": "/bin/sh" }),
    )
    .await;
    assert_eq!(unknown["isError"], true);
    assert!(
        body(&unknown)["error"].as_str().unwrap().contains("executable"),
        "{unknown}"
    );

    let launched = call(
        AGENT_LAUNCH,
        json!({ "harness": "claude", "brief": "Wait.", "working_dir": "one" }),
    )
    .await;
    assert_eq!(launched["isError"], false, "{launched}");
    let launched = body(&launched);
    let agent = launched["agent"].as_str().unwrap();
    assert!(agent.starts_with("launched-claude-"), "{launched}");
    assert_eq!(launched["depth"], 1);
    assert!(launched.get("thread").is_some(), "{launched}");

    // One worktree, one agent: a second launch into it is refused while the first runs.
    let shared = call(
        AGENT_LAUNCH,
        json!({ "harness": "claude", "brief": "Wait.", "working_dir": "one" }),
    )
    .await;
    assert_eq!(body(&shared)["error"], "working_dir_in_use", "{shared}");

    let session = launched["session"].as_u64().unwrap();
    let stopped = call(AGENT_STOP, json!({ "session": session })).await;
    assert_eq!(stopped["isError"], false, "{stopped}");
    let unknown = call(AGENT_STOP, json!({ "session": session + 100 })).await;
    assert_eq!(body(&unknown)["error"], "unknown_session");
    let id = SessionId::from_raw(session);
    let state = host.supervisor.wait(id, std::time::Duration::from_secs(10));
    assert!(matches!(state, Some(SessionState::Finished(_))), "{state:?}");
    // Once it finished, its worktree is free again.
    let again = call(
        AGENT_LAUNCH,
        json!({ "harness": "claude", "brief": "Wait.", "working_dir": "one" }),
    )
    .await;
    assert_eq!(again["isError"], false, "{again}");
}

/// Review 3 S1: two leads of one user share the worktree locks, so neither launches into a
/// worktree the other holds, nor into one inside or around it; a finished session lets go.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_leads_never_launch_into_the_same_or_nested_worktrees() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.worktrees.join("one/sub")).unwrap();
    let first = Arc::new(fixture.start(fixture.policy(), fixture.base.join("audit-1")).unwrap());
    let second = Arc::new(fixture.start(fixture.policy(), fixture.base.join("audit-2")).unwrap());
    let launch = |host: &Arc<LeadHost>, dir: &str| {
        Arc::clone(host).call(
            AGENT_LAUNCH,
            json!({ "harness": "claude", "brief": "Wait.", "working_dir": dir }),
        )
    };

    let launched = launch(&first, "one/sub").await;
    assert_eq!(launched["isError"], false, "{launched}");
    for dir in ["one/sub", "one"] {
        let refused = launch(&second, dir).await;
        assert_eq!(body(&refused)["error"], "working_dir_in_use", "{dir}: {refused}");
    }
    let sibling = launch(&second, "two").await;
    assert_eq!(sibling["isError"], false, "{sibling}");

    let session = body(&launched)["session"].as_u64().unwrap();
    let stopped = Arc::clone(&first).call(AGENT_STOP, json!({ "session": session })).await;
    assert_eq!(stopped["isError"], false, "{stopped}");
    let state = first
        .supervisor
        .wait(SessionId::from_raw(session), std::time::Duration::from_secs(10));
    assert!(matches!(state, Some(SessionState::Finished(_))), "{state:?}");
    // The observer lets go of the locks before the session is marked finished.
    let again = launch(&second, "one").await;
    assert_eq!(again["isError"], false, "the worktree stayed locked: {again}");
}

/// Two leads whose worktrees roots nest: the outer lead may not launch into a directory that
/// holds the inner lead's live worktree, though that directory lies above the inner lead's root.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leads_with_nested_worktrees_roots_never_launch_around_each_others_worktrees() {
    let fixture = Fixture::new();
    let inner_root = fixture.worktrees.join("deep/inner");
    std::fs::create_dir_all(inner_root.join("x")).unwrap();
    let inner = Arc::new(
        fixture
            .start(
                fixture.policy_at(&inner_root, "inner.toml"),
                fixture.base.join("audit-1"),
            )
            .unwrap(),
    );
    let outer = Arc::new(fixture.start(fixture.policy(), fixture.base.join("audit-2")).unwrap());
    let launch = |host: &Arc<LeadHost>, dir: &str| {
        Arc::clone(host).call(
            AGENT_LAUNCH,
            json!({ "harness": "claude", "brief": "Wait.", "working_dir": dir }),
        )
    };
    let launched = launch(&inner, "x").await;
    assert_eq!(launched["isError"], false, "{launched}");
    for dir in ["deep", "deep/inner", "deep/inner/x"] {
        let refused = launch(&outer, dir).await;
        assert_eq!(body(&refused)["error"], "working_dir_in_use", "{dir}: {refused}");
    }
    let sibling = launch(&outer, "two").await;
    assert_eq!(sibling["isError"], false, "{sibling}");
    for host in [&inner, &outer] {
        host.shutdown();
    }
}

/// Each lock file is open once in this process, however many sessions hold it, and a refused
/// claim closes none that a session holds: closing any descriptor of a file releases the
/// process's `fcntl` lock on it.
#[test]
fn a_lock_file_is_open_once_per_process_and_a_refused_claim_keeps_the_others() {
    let fixture = Fixture::new();
    let locks = fixture.base.join("locks-once");
    for dir in ["one/a", "one/b"] {
        std::fs::create_dir_all(fixture.worktrees.join(dir)).unwrap();
    }
    let a = lock_worktree(&locks, &fixture.worktrees.join("one/a")).unwrap();
    let b = lock_worktree(&locks, &fixture.worktrees.join("one/b")).unwrap();
    // The ancestors' files, `/` first, are the same descriptors.
    let shared = a.0.len() - 1;
    for at in 0..shared {
        assert!(Arc::ptr_eq(&a.0[at], &b.0[at]), "ancestor {at} opened twice");
    }
    assert_eq!(
        lock_worktree(&locks, &fixture.worktrees.join("one")).err().as_deref(),
        Some("working_dir_in_use")
    );
    assert_eq!(
        lock_worktree(&locks, &fixture.worktrees.join("one/a")).err().as_deref(),
        Some("working_dir_in_use")
    );
    // The refused claims closed nothing `a` and `b` hold.
    for file in a.0.iter().chain(&b.0) {
        assert!(Arc::strong_count(file) >= 1);
        // SAFETY: fcntl F_GETFD only reads the descriptor's flags.
        assert_ne!(
            unsafe { libc::fcntl(std::os::unix::io::AsRawFd::as_raw_fd(&**file), libc::F_GETFD) },
            -1
        );
    }
    drop(a);
    assert!(lock_worktree(&locks, &fixture.worktrees.join("one/a")).is_ok());
}

/// A child this process forked, blocked until [`Forked::release`]. Between fork and exec (a
/// `pre_exec` hook, or a slow `exec`) every launch is such a child.
struct Forked {
    pid: libc::pid_t,
    release: std::os::fd::OwnedFd,
}

impl Forked {
    /// Fork. The child first locks `lock` exclusively when given, and the call returns whether it
    /// got the lock. The child runs only async-signal-safe calls: this process has other threads.
    fn new(lock: Option<&std::ffi::CStr>) -> (Self, bool) {
        use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
        let pipe = || {
            let mut fds = [0; 2];
            // SAFETY: pipe fills the two descriptors, which are then owned here.
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            // SAFETY: fresh descriptors nothing else owns.
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
        };
        let (release_r, release_w) = pipe();
        let (report_r, report_w) = pipe();
        // SAFETY: plain integers; all zeroes is the whole file.
        let mut range: libc::flock = unsafe { std::mem::zeroed() };
        range.l_type = libc::F_WRLCK as libc::c_short;
        range.l_whence = libc::SEEK_SET as libc::c_short;
        // SAFETY: the child calls only open, fcntl, write, read and _exit.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            let mut got = 0u8;
            if let Some(path) = lock {
                // SAFETY: async-signal-safe calls on a path and a struct prepared before the fork.
                unsafe {
                    let fd = libc::open(path.as_ptr(), libc::O_RDWR);
                    if fd >= 0 && libc::fcntl(fd, libc::F_SETLK, &raw const range) == 0 {
                        got = 1;
                    }
                }
            }
            // SAFETY: async-signal-safe calls on descriptors inherited from the parent.
            unsafe {
                libc::write(report_w.as_raw_fd(), (&raw const got).cast(), 1);
                let mut byte = 0u8;
                libc::read(release_r.as_raw_fd(), (&raw mut byte).cast(), 1);
                libc::_exit(0);
            }
        }
        drop((release_r, report_w));
        let mut got = 0u8;
        // SAFETY: reads one byte into a local from a descriptor owned here.
        assert_eq!(unsafe { libc::read(report_r.as_raw_fd(), (&raw mut got).cast(), 1) }, 1);
        (
            Self {
                pid,
                release: release_w,
            },
            got == 1,
        )
    }

    fn release(self) {
        use std::os::fd::AsRawFd as _;
        let byte = 0u8;
        // SAFETY: writes one byte from a local, then reaps the child this value owns.
        unsafe {
            libc::write(self.release.as_raw_fd(), (&raw const byte).cast(), 1);
            let mut status = 0;
            assert_eq!(libc::waitpid(self.pid, &raw mut status, 0), self.pid);
        }
    }
}

/// A worktree lock let go while a child this process forked has not exec'd yet is free: the child
/// holds a copy of every descriptor, so a lock that belonged to the descriptor (`flock`) would
/// outlive its release and refuse the next claim of a free worktree.
#[test]
fn a_released_worktree_lock_is_free_while_a_forked_child_runs() {
    let fixture = Fixture::new();
    let locks = fixture.base.join("locks-fork");
    let dir = fixture.worktrees.join("one");
    let held = lock_worktree(&locks, &dir).unwrap();
    let (child, _) = Forked::new(None);
    drop(held);
    let again = lock_worktree(&locks, &dir);
    child.release();
    assert!(again.is_ok(), "{:?}", again.err());
}

/// The locks exclude another process: a worktree another process holds, or one inside it, is
/// refused until that process lets go.
#[test]
fn a_worktree_another_process_holds_is_refused_until_it_lets_go() {
    let fixture = Fixture::new();
    let locks = fixture.base.join("locks-other");
    std::fs::create_dir_all(fixture.worktrees.join("one/sub")).unwrap();
    let dir = fixture.worktrees.join("one");
    // Created by a claim, so the child has a file to lock.
    drop(lock_worktree(&locks, &dir).unwrap());
    let path = std::ffi::CString::new(lock_file(&locks, &dir).into_os_string().into_encoded_bytes()).unwrap();
    let (child, locked) = Forked::new(Some(&path));
    assert!(locked, "the child could not lock the worktree");
    let refused = [dir.clone(), dir.join("sub")].map(|dir| lock_worktree(&locks, &dir).err());
    child.release();
    assert_eq!(
        refused,
        [
            Some("working_dir_in_use".to_owned()),
            Some("working_dir_in_use".to_owned())
        ]
    );
    assert!(lock_worktree(&locks, &dir).is_ok());
}

/// Review 3 N1: an old audit log of an ended lead is deleted when a lead starts; a recent one, and
/// one a live lead still holds, are kept.
#[test]
fn old_audit_logs_of_ended_leads_are_pruned() {
    use std::os::unix::io::AsRawFd as _;
    let fixture = Fixture::new();
    let dir = fixture.base.join("audit-prune");
    std::fs::create_dir_all(&dir).unwrap();
    let old = dir.join("launch-audit-1-1.jsonl");
    let held = dir.join("launch-audit-2-1.jsonl");
    let recent = dir.join("launch-audit-3-1.jsonl");
    let other = dir.join("notes.jsonl");
    for path in [&old, &held, &recent, &other] {
        std::fs::write(path, "{}\n").unwrap();
    }
    let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(40 * 24 * 60 * 60);
    for path in [&old, &held, &other] {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
    }
    let holder = std::fs::File::open(&held).unwrap();
    // SAFETY: flock on a descriptor this test owns.
    assert_eq!(
        unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    prune_audit_logs(&dir, AUDIT_RETENTION);
    assert!(!old.exists());
    assert!(held.exists() && recent.exists() && other.exists());
}
