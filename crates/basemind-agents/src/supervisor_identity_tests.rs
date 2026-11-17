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
        let away = crate::procinfo::info(pid)
            .as_ref()
            .and_then(crate::procinfo::ancestors)
            .is_some_and(|chain| chain.procs.iter().all(|proc| proc.pid != me));
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
    // Placed outside every session and every registered process, for where it is, not merely
    // because a link of its ancestry could not be read: the walk reached init, or on macOS
    // stopped only at launchd (pid 1, root's, which it does not describe).
    let placed = crate::supervisor_identity::origin(&escaped.supervisor.inner.table(), &escaped.peer);
    let me = crate::procinfo::info(escaped.peer.id.pid).unwrap();
    let chain = crate::procinfo::ancestors(&me).unwrap();
    let top = chain.procs.last().unwrap_or(&me);
    let expected = crate::supervisor_identity::Origin::Outside {
        complete: chain.complete,
    };
    assert_eq!(placed, expected, "the escaped process was misplaced");
    assert!(
        chain.complete || (cfg!(target_os = "macos") && top.ppid == 1),
        "the escaped process's ancestry stopped below init, at {}",
        top.ppid
    );
    let claim = Caller::principal("lead", escaped.peer).unwrap();
    let launched = escaped.supervisor.launch(&claim, HarnessKind::ClaudeCode, &request());
    assert_eq!(
        launched.map(|launched| launched.depth).map_err(|error| error.code()),
        Err("peer_unverified"),
        "an escaped process launched as the configured launcher"
    );
    if let Some(session) = &escaped.launched {
        // Even a caller that somehow carried the operator kind is refused from that process.
        assert_eq!(
            escaped
                .supervisor
                .stop(&forged_operator(escaped.peer), session.id)
                .map_err(|error| error.code()),
            Err("peer_unverified"),
            "an escaped process acted as the operator"
        );
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
fn a_principal_is_its_registered_child_and_its_descendants_only() {
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
        supervisor.operator().map(|_| ()).map_err(|error| error.code()),
        Err("caller_not_admitted")
    );
    // A process that is not a launcher the policy names cannot be registered as one.
    assert_eq!(
        supervisor
            .register_launcher_in_process("stranger")
            .map_err(|error| error.code()),
        Err("caller_not_admitted")
    );

    // A child the embedding spawned, with a child of its own, as a tool it ran would be.
    let parent = std::process::Command::new(&perl)
        .args(["-e", "my $c = fork; if (!$c) { sleep 60; exit } sleep 60"])
        .spawn()
        .unwrap();
    let mut parent = Stand(parent);
    let deadline = Instant::now() + Duration::from_secs(5);
    let child = loop {
        let all = crate::procinfo::all().unwrap();
        if let Some(child) = all
            .iter()
            .find(|proc| u32::try_from(proc.ppid).ok() == Some(parent.0.id()))
        {
            break Peer::of_pid(u32::try_from(child.pid).unwrap());
        }
        assert!(Instant::now() < deadline, "the registered child's child never started");
        std::thread::sleep(Duration::from_millis(10));
    };
    let _reclaim_child = Reclaim(child);
    supervisor.register_launcher_child("lead", &mut parent.0).unwrap();
    // A binding is write-once.
    let mut other = Stand::new();
    assert_eq!(
        supervisor
            .register_launcher_child("lead", &mut other.0)
            .map_err(|error| error.code()),
        Err("role_bound")
    );
    // Its descendant is the lead.
    let as_child = Caller::principal("lead", child).unwrap();
    let launched = supervisor
        .launch(&as_child, HarnessKind::ClaudeCode, &request())
        .unwrap();
    assert_eq!(launched.depth, 1);
    // A descendant of a registered child is never the operator, whatever its caller says.
    supervisor.register_operator_in_process().unwrap();
    assert_eq!(
        supervisor
            .stop(&forged_operator(child), launched.id)
            .map_err(|error| error.code()),
        Err("caller_not_admitted"),
        "a registered child's descendant acted as the operator"
    );
    // A process below the supervisor but not below the lead is nobody.
    let sibling = Stand::new();
    let as_sibling = Caller::principal("lead", sibling.peer()).unwrap();
    assert_eq!(
        supervisor.stop(&as_sibling, launched.id).map_err(|error| error.code()),
        Err("caller_not_admitted")
    );
    // Released, the binding admits nobody: a caller already made is refused.
    assert!(supervisor.unregister_launcher("lead"));
    assert!(!supervisor.unregister_launcher("lead"));
    assert_eq!(
        supervisor.stop(&as_child, launched.id).map_err(|error| error.code()),
        Err("caller_not_admitted"),
        "a caller of a released binding was still the principal"
    );
    // Gone, the bound process still holds the role: only an unregistration releases it.
    supervisor.register_launcher_child("lead", &mut parent.0).unwrap();
    drop(parent);
    assert_eq!(
        supervisor
            .register_launcher_child("lead", &mut other.0)
            .map_err(|error| error.code()),
        Err("role_bound"),
        "a dead binding was replaced without an unregistration"
    );
    assert!(supervisor.unregister_launcher("lead"));
    supervisor.register_launcher_child("lead", &mut other.0).unwrap();
    assert_eq!(
        outcomes(&audit, AuditEvent::Registration),
        [
            "caller_not_admitted",
            "registered",
            "role_bound",
            "registered",
            "unregistered",
            "registered",
            "role_bound",
            "unregistered",
            "registered"
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
fn a_child_that_has_exited_cannot_be_registered() {
    let scratch = Scratch::new();
    let supervisor = Supervisor::new(
        LaunchPolicy::new(policy(&[HarnessKind::ClaudeCode])).unwrap(),
        vec![scratch.config(HarnessKind::ClaudeCode, &scratch.long())],
        registry(),
        Arc::new(MemoryAuditSink::default()),
    )
    .unwrap();
    // Reaped already: its pid may name any process now.
    let mut reaped = std::process::Command::new("/usr/bin/true").spawn().unwrap();
    reaped.wait().unwrap();
    // Exited and not reaped: `try_wait` reaps it, and its pid is free from then on.
    let mut exited = std::process::Command::new("/usr/bin/true").spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while crate::procinfo::info(i32::try_from(exited.id()).unwrap()).is_some_and(|now| !now.zombie) {
        assert!(Instant::now() < deadline, "the child never exited");
        std::thread::sleep(Duration::from_millis(10));
    }
    for child in [&mut reaped, &mut exited] {
        assert_eq!(
            supervisor
                .register_launcher_child("lead", child)
                .map_err(|error| error.code()),
            Err("peer_unverified"),
            "an exited child was registered"
        );
    }
    // A running child is accepted.
    let mut running = Stand::new();
    supervisor.register_launcher_child("lead", &mut running.0).unwrap();
}

/// Which part of [`a_subreaper_ancestor_adopts_no_escapee_as_launcher`] this
/// process plays: unset for the test itself, `wrapper` for the subreaper it runs under, `inner`
/// for the supervisor below it.
#[cfg(target_os = "linux")]
const SUBREAPER_ROLE: &str = "BASEMIND_TEST_SUBREAPER_ROLE";
/// The file the inner run writes once its assertions passed, so a rerun that matched no test
/// (and exits 0) fails the outer one.
#[cfg(target_os = "linux")]
const SUBREAPER_MARKER: &str = "BASEMIND_TEST_SUBREAPER_MARKER";

/// On Linux an orphan is reparented to the nearest child subreaper among its dead parent's
/// ancestors. With a subreaper above the supervisor, a session's double-forked process lands
/// below it: were it registered, the escapee would descend from a registered process.
#[cfg(target_os = "linux")]
#[test]
fn a_subreaper_ancestor_adopts_no_escapee_as_launcher() {
    const NAME: &str = "supervisor::identity_tests::a_subreaper_ancestor_adopts_no_escapee_as_launcher";
    // The wrapper's rerun inherits the marker's path from the environment it was given.
    let rerun = |role: &str, marker: Option<&Path>| {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([NAME, "--exact", "--nocapture", "--test-threads=1"])
            .env(SUBREAPER_ROLE, role);
        if let Some(marker) = marker {
            command.env(SUBREAPER_MARKER, marker);
        }
        command.status().unwrap()
    };
    match std::env::var(SUBREAPER_ROLE).as_deref() {
        Err(_) => {
            if perl().is_none() {
                return;
            }
            let scratch = Scratch::new();
            let marker = scratch.path.join("inner-ran");
            assert!(
                rerun("wrapper", Some(&marker)).success(),
                "the run below the subreaper failed"
            );
            assert!(
                marker.exists(),
                "the run below the subreaper ran no test: {NAME} matched nothing"
            );
        }
        Ok("wrapper") => {
            // SAFETY: prctl with PR_SET_CHILD_SUBREAPER takes plain integers.
            let set = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
            assert_eq!(set, 0, "{}", std::io::Error::last_os_error());
            let status = rerun("inner", None);
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
            assert_escape_refused(&escaped);
            std::fs::write(std::env::var_os(SUBREAPER_MARKER).unwrap(), "ran").unwrap();
        }
    }
}
