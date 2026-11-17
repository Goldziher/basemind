//! Escapes from a session and the registration of launchers and the operator, reproduced with
//! stand-in harnesses. Every process a test signals is one it started, checked by pid and start
//! time.

use std::path::Path;

use super::attack_tests::{Reclaim, Stand, perl};
use super::tests::*;
use super::*;
use crate::audit::MemoryAuditSink;
use crate::event::FinishStatus;
use crate::peer::Peer;

/// A process that got outside the supervisor's tree by `escape` (a perl program that writes its
/// own pid to `escaped.pid` in the scratch directory), killed at the end of the test.
struct Escaped {
    scratch: Scratch,
    supervisor: Supervisor,
    launched: Option<Launched>,
    peer: Peer,
    _reclaim: Reclaim,
}

/// Waits for `escaped.pid`, then for that process to be reparented out of this process's tree.
fn escapee(dir: &Path) -> Peer {
    let pid = recorded_pid(&dir.join("escaped.pid"));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(crate::procinfo::info(pid).is_some(), "the escaped process is gone");
        let me = i32::try_from(std::process::id()).unwrap();
        let away = crate::procinfo::ancestors(pid).is_some_and(|chain| chain.procs.iter().all(|proc| proc.pid != me));
        if away {
            return Peer::of_pid(u32::try_from(pid).unwrap());
        }
        assert!(Instant::now() < deadline, "the escaped process kept its parent");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A Claude Code session (not tracked for detached processes) whose stand-in runs `perl
/// <escape>` in the background, then either keeps running or completes its turn and ends.
fn escape_from_session(escape: &str, end_session: bool) -> Option<Escaped> {
    let perl = perl()?;
    let scratch = Scratch::new();
    let tail = if end_session {
        format!("sleep 1\n{RESULT}")
    } else {
        "cat > /dev/null\n".to_owned()
    };
    let body = format!(
        "'{perl}' {escape} &\n{HANDSHAKE}{tail}",
        perl = perl.display(),
        escape = escape.replace("{dir}", &scratch.path.display().to_string()),
    );
    let (supervisor, _) = start(
        policy(&[HarnessKind::ClaudeCode]),
        vec![scratch.config(HarnessKind::ClaudeCode, &body)],
    );
    let launched = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request()).unwrap();
    if end_session {
        assert_eq!(finished(&supervisor, launched.id), FinishStatus::Completed);
    }
    let peer = escapee(&scratch.path);
    Some(Escaped {
        scratch,
        supervisor,
        launched: Some(launched),
        peer,
        _reclaim: Reclaim(peer),
    })
}

/// Asserts that `escaped` is neither the configured launcher nor the operator.
fn assert_escape_refused(escaped: &Escaped) {
    // Placed outside every session and every registered process, not merely unreadable.
    let placed = crate::supervisor_identity::origin(&escaped.supervisor.inner.table(), &escaped.peer);
    assert!(
        matches!(placed, crate::supervisor_identity::Origin::Outside { .. }),
        "the escaped process was placed as {placed:?}"
    );
    let claim = Caller::principal("lead", escaped.peer).unwrap();
    let launched = escaped.supervisor.launch(&claim, HarnessKind::ClaudeCode, &request());
    assert_eq!(
        launched.map(|launched| launched.depth).map_err(|error| error.code()),
        Err("peer_unverified"),
        "an escaped process launched as the configured launcher"
    );
    let operator = escaped.supervisor.operator(escaped.peer);
    assert_eq!(
        operator.map(|_| ()).map_err(|error| error.code()),
        Err("peer_unverified"),
        "an escaped process was made the operator"
    );
    if let Some(session) = &escaped.launched {
        assert_eq!(
            escaped
                .supervisor
                .stop(&claim, session.id)
                .map_err(|error| error.code()),
            Err("peer_unverified")
        );
    }
    let _ = &escaped.scratch;
}

#[test]
fn a_process_orphaned_past_its_sessions_end_is_no_launcher_or_operator() {
    // Leaves the harness's group so the session's kill misses it, and outlives the session.
    let escape = "-e 'setpgrp(0, 0); open(my $o, q{>}, q{{dir}/escaped.pid}) or die; print $o $$; close $o; sleep 300'";
    let Some(escaped) = escape_from_session(escape, true) else {
        return;
    };
    assert_escape_refused(&escaped);
}

#[test]
fn a_double_forked_process_in_a_session_of_its_own_is_no_launcher_or_operator() {
    // setsid in a child whose parent then exits: reparented to init, its ancestry gone.
    let escape = "-MPOSIX -e 'fork and exit; POSIX::setsid(); fork and exit; \
                  open(my $o, q{>}, q{{dir}/escaped.pid}) or die; print $o $$; close $o; sleep 300'";
    let Some(escaped) = escape_from_session(escape, false) else {
        return;
    };
    assert_escape_refused(&escaped);
}

#[test]
fn a_process_an_outside_spawner_started_is_no_launcher_or_operator() {
    // A same-user process outside both the supervisor's tree and the registered launcher's (as
    // tmux, launchd or ssh would start one on an agent's behalf).
    let Some(perl) = perl() else {
        return;
    };
    let scratch = Scratch::new();
    let status = std::process::Command::new(&perl)
        .arg("-MPOSIX")
        .arg("-e")
        .arg(format!(
            "fork and exit; POSIX::setsid(); fork and exit; open(my $o, q{{>}}, q{{{}/escaped.pid}}) or die; \
             print $o $$; close $o; sleep 300",
            scratch.path.display()
        ))
        .status()
        .unwrap();
    assert!(status.success());
    let (supervisor, _) = start(
        policy(&[HarnessKind::ClaudeCode]),
        vec![scratch.config(HarnessKind::ClaudeCode, &scratch.long())],
    );
    let peer = escapee(&scratch.path);
    let escaped = Escaped {
        scratch,
        supervisor,
        launched: None,
        peer,
        _reclaim: Reclaim(peer),
    };
    assert_escape_refused(&escaped);
}

#[test]
fn a_principal_is_its_registered_process_and_its_descendants_only() {
    let Some(perl) = perl() else {
        return;
    };
    let scratch = Scratch::new();
    let audit = Arc::new(MemoryAuditSink::default());
    // Nothing registered: the default denies every principal and the operator.
    let supervisor = Supervisor::new(
        LaunchPolicy::new(policy(&[HarnessKind::ClaudeCode])).unwrap(),
        vec![scratch.config(HarnessKind::ClaudeCode, &scratch.long())],
        registry(),
        audit.clone(),
    )
    .unwrap();
    let refused = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request());
    assert_eq!(refused.err().map(|error| error.code()), Some("peer_unverified"));
    assert_eq!(
        supervisor.operator(own()).map(|_| ()).map_err(|error| error.code()),
        Err("peer_unverified")
    );
    // A process that is not a launcher the policy names cannot be registered as one.
    assert_eq!(
        supervisor
            .register_launcher("stranger", own())
            .map_err(|error| error.code()),
        Err("caller_not_admitted")
    );

    // The lead runs as a process of its own, with a child it started, as a tool would be.
    let parent = std::process::Command::new(&perl)
        .args(["-e", "my $c = fork; if (!$c) { sleep 60; exit } sleep 60"])
        .spawn()
        .unwrap();
    let parent = Stand(parent);
    let deadline = Instant::now() + Duration::from_secs(5);
    let child = loop {
        let all = crate::procinfo::all().unwrap();
        if let Some(child) = all
            .iter()
            .find(|proc| u32::try_from(proc.ppid).ok() == Some(parent.0.id()))
        {
            break Peer::of_pid(u32::try_from(child.pid).unwrap());
        }
        assert!(Instant::now() < deadline, "the lead's child never started");
        std::thread::sleep(Duration::from_millis(10));
    };
    let _reclaim_child = Reclaim(child);
    // By peer, a process below the supervisor is refused; as the child the embedding spawned
    // it is accepted.
    assert_eq!(
        supervisor
            .register_launcher("lead", parent.peer())
            .map_err(|error| error.code()),
        Err("caller_not_admitted")
    );
    supervisor.register_launcher_child("lead", &parent.0).unwrap();
    supervisor.register_operator_child(&parent.0).unwrap();
    // A binding is write-once while its process runs.
    let other = Stand::new();
    assert_eq!(
        supervisor
            .register_launcher_child("lead", &other.0)
            .map_err(|error| error.code()),
        Err("role_bound")
    );
    // Its descendant is the lead, and may be made the operator.
    let as_child = Caller::principal("lead", child).unwrap();
    let launched = supervisor
        .launch(&as_child, HarnessKind::ClaudeCode, &request())
        .unwrap();
    assert_eq!(launched.depth, 1);
    let operator = supervisor.operator(child).unwrap();
    // A process below the supervisor but not below the lead is nobody.
    let sibling = Stand::new();
    let as_sibling = Caller::principal("lead", sibling.peer()).unwrap();
    assert_eq!(
        supervisor.stop(&as_sibling, launched.id).map_err(|error| error.code()),
        Err("caller_not_admitted")
    );
    assert_eq!(
        supervisor
            .operator(sibling.peer())
            .map(|_| ())
            .map_err(|error| error.code()),
        Err("caller_not_admitted")
    );
    // A session's process cannot be registered.
    assert_eq!(
        supervisor
            .register_launcher("second", inside(&supervisor, launched.id))
            .map_err(|error| error.code()),
        Err("caller_not_admitted")
    );
    // The operator stays bound to the process, not to the caller value: once that process is
    // gone the minted operator is refused.
    supervisor.stop(&operator, launched.id).unwrap();
    assert_eq!(finished(&supervisor, launched.id), FinishStatus::Stopped);
    drop(parent);
    let gone = supervisor.stop(&operator, launched.id);
    assert!(gone.is_err(), "an operator outlived its registered process");
    // Once the bound process is gone the role can be bound again.
    supervisor.register_launcher_child("lead", &other.0).unwrap();
    let registrations: Vec<&str> = audit
        .records()
        .iter()
        .filter(|record| record.event == AuditEvent::Registration)
        .map(|record| record.outcome)
        .collect();
    assert_eq!(
        registrations,
        [
            "caller_not_admitted",
            "caller_not_admitted",
            "registered",
            "registered",
            "role_bound",
            "caller_not_admitted",
            "replaced"
        ]
    );
}

#[test]
fn the_peer_of_a_connection_from_another_process_is_that_process() {
    use std::os::fd::AsFd;
    let Some(perl) = perl() else {
        return;
    };
    let scratch = Scratch::new();
    let path = scratch.path.join("agents.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let client = std::process::Command::new(&perl)
        .arg("-MIO::Socket::UNIX")
        .arg("-e")
        .arg(format!(
            "my $s = IO::Socket::UNIX->new(Peer => q{{{}}}) or die; sleep 30",
            path.display()
        ))
        .spawn()
        .unwrap();
    let client = Stand(client);
    let (connection, _) = listener.accept().unwrap();
    let peer = Peer::of_socket(connection.as_fd()).unwrap();
    assert_eq!(peer.pid(), client.0.id());
    assert_eq!(peer, client.peer());
}

#[test]
fn an_ancestor_of_the_supervisor_or_a_process_below_it_cannot_be_registered_by_peer() {
    let scratch = Scratch::new();
    let audit = Arc::new(MemoryAuditSink::default());
    let supervisor = Supervisor::new(
        LaunchPolicy::new(policy(&[HarnessKind::ClaudeCode])).unwrap(),
        vec![scratch.config(HarnessKind::ClaudeCode, &scratch.long())],
        registry(),
        audit,
    )
    .unwrap();
    // The test runner's parent: an orphan of a session can be reparented to an ancestor (a
    // Linux subreaper), and would then descend from the registered process.
    let ancestor = Peer::of_pid(std::os::unix::process::parent_id());
    assert_eq!(
        supervisor
            .register_launcher("lead", ancestor)
            .map_err(|error| error.code()),
        Err("caller_not_admitted"),
        "an ancestor of the supervisor was registered"
    );
    // A process below the supervisor, as a starting session's harness would be.
    let below = Stand::new();
    assert_eq!(
        supervisor.register_operator(below.peer()).map_err(|error| error.code()),
        Err("caller_not_admitted"),
        "a process below the supervisor was registered by its peer"
    );
}

/// Which part of [`a_subreaper_ancestor_cannot_be_registered_and_adopts_no_launcher`] this
/// process plays: unset for the test itself, `wrapper` for the subreaper it runs under, `inner`
/// for the supervisor below it.
#[cfg(target_os = "linux")]
const SUBREAPER_ROLE: &str = "BASEMIND_TEST_SUBREAPER_ROLE";

/// On Linux an orphan is reparented to the nearest child subreaper among its dead parent's
/// ancestors. With a subreaper above the supervisor, a session's double-forked process lands
/// below it: were it registered, the escapee would descend from a registered process.
#[cfg(target_os = "linux")]
#[test]
fn a_subreaper_ancestor_cannot_be_registered_and_adopts_no_launcher() {
    const NAME: &str = "supervisor::identity_tests::a_subreaper_ancestor_cannot_be_registered_and_adopts_no_launcher";
    let rerun = |role: &str| {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([NAME, "--exact", "--nocapture", "--test-threads=1"])
            .env(SUBREAPER_ROLE, role)
            .status()
            .unwrap()
    };
    match std::env::var(SUBREAPER_ROLE).as_deref() {
        Err(_) => assert!(rerun("wrapper").success(), "the run below the subreaper failed"),
        Ok("wrapper") => {
            // SAFETY: prctl with PR_SET_CHILD_SUBREAPER takes plain integers.
            let set = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
            assert_eq!(set, 0, "{}", std::io::Error::last_os_error());
            let status = rerun("inner");
            std::process::exit(status.code().unwrap_or(1));
        }
        Ok(_) => {
            let wrapper = Peer::of_pid(std::os::unix::process::parent_id());
            let escape = "-MPOSIX -e 'fork and exit; POSIX::setsid(); fork and exit; \
                          open(my $o, q{>}, q{{dir}/escaped.pid}) or die; print $o $$; close $o; sleep 300'";
            let Some(escaped) = escape_from_session(escape, false) else {
                return;
            };
            // The escapee was adopted by the subreaper above the supervisor.
            let adopted = crate::procinfo::info(escaped.peer.id.pid).unwrap();
            assert_eq!(adopted.ppid, wrapper.id.pid, "the subreaper did not adopt the escapee");
            assert_eq!(
                escaped
                    .supervisor
                    .register_launcher("lead", wrapper)
                    .map_err(|error| error.code()),
                Err("caller_not_admitted"),
                "the subreaper above the supervisor was registered"
            );
            assert_escape_refused(&escaped);
        }
    }
}
