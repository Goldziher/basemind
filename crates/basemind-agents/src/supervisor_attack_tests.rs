//! Attacks on the supervisor by a hostile launched agent with a shell, reproduced with
//! stand-in harnesses: identity forgery and escape, the operator, the detached reaper, and
//! panics that would leak a session's slot. No real harness is started, and every process a
//! test signals is one it started, checked by pid and start time.

use std::path::{Path, PathBuf};

use super::tests::*;
use super::*;
use crate::audit::MemoryAuditSink;
use crate::claude::ClaudeCodeAdapter;
use crate::event::{Event, FinishStatus};
use crate::peer::Peer;
use crate::policy::{FlavorPolicy, PolicySpec};
use crate::registry::Adapter;
use crate::session::{CommandPlan, Session};

/// `perl`, which the attack stand-ins use to call `setpgid` and `setsid`. Its absence fails the
/// test unless `BASEMIND_TEST_ALLOW_NO_SETSID` is set, so a missing tool never passes silently.
fn perl() -> Option<PathBuf> {
    let found = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("perl"))
        .find(|candidate| candidate.is_file());
    if found.is_none() {
        assert!(
            std::env::var_os("BASEMIND_TEST_ALLOW_NO_SETSID").is_some(),
            "perl is not installed; set BASEMIND_TEST_ALLOW_NO_SETSID=1 to skip the tests that need it"
        );
        eprintln!("skipped: perl is not installed and BASEMIND_TEST_ALLOW_NO_SETSID is set");
    }
    found
}

/// Lead launches A (Claude Code); A launches C (OpenCode), whose stand-in runs `perl <attack>`
/// in the background. Returns the supervisor, both sessions and the attacking process's pid.
/// `attack` is given the scratch directory, where `a.pid` holds A's pid.
struct Lateral {
    scratch: Scratch,
    supervisor: Supervisor,
    a: Launched,
    c: Launched,
    attacker: u32,
    /// The attacker as it started, so it is killed at the end only while it is still that
    /// process.
    started: Peer,
}

impl Drop for Lateral {
    fn drop(&mut self) {
        let pid = self.started.id.pid;
        if crate::procinfo::info(pid).is_some_and(|now| now.start == self.started.id.start) {
            // SAFETY: the test started this process, and its start time was checked just now.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

fn lateral(attack: impl Fn(&Path) -> String) -> Option<Lateral> {
    let perl = perl()?;
    let scratch = Scratch::new();
    let pidfile = scratch.path.join("attacker.pid");
    let body = format!(
        "{HANDSHAKE}'{perl}' {attack} &\necho $! > '{pidfile}'\ncat > /dev/null\n",
        perl = perl.display(),
        attack = attack(&scratch.path),
        pidfile = pidfile.display(),
    );
    let (supervisor, _) = start(
        PolicySpec {
            max_depth: 3,
            ..policy(&[HarnessKind::ClaudeCode, OPENCODE])
        },
        vec![
            scratch.config(HarnessKind::ClaudeCode, &scratch.long()),
            scratch.config(OPENCODE, &body),
        ],
    );
    let a = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request()).unwrap();
    std::fs::write(scratch.path.join("a.pid"), pid_of(&supervisor, a.id).to_string()).unwrap();
    let token = scratch.env_of(&a.agent)["BASEMIND_LAUNCH_TOKEN"].clone();
    let c = supervisor
        .launch(
            &Caller::launched(&token, inside(&supervisor, a.id)),
            OPENCODE,
            &request(),
        )
        .unwrap();
    let attacker = u32::try_from(recorded_pid(&pidfile)).unwrap();
    Some(Lateral {
        scratch,
        supervisor,
        a,
        c,
        attacker,
        started: Peer::of_pid(attacker),
    })
}

/// What the attacker wrote to `outcome` in the scratch directory, once it has.
fn attack_outcome(scratch: &Scratch) -> String {
    let path = scratch.path.join("outcome");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        if !text.is_empty() {
            return text;
        }
        assert!(Instant::now() < deadline, "the attacker never reported");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_child_cannot_join_its_launchers_group_and_act_as_its_launcher() {
    // C's tool process moves into A's process group, then claims to be the configured launcher.
    let Some(attack) = lateral(|dir| {
        format!(
            "-e 'open(my $f, q{{<}}, q{{{dir}/a.pid}}) or die; my $a = <$f>; chomp $a; \
             my $r = setpgrp(0, $a) ? q{{joined}} : q{{refused}}; \
             open(my $o, q{{>}}, q{{{dir}/outcome}}) or die; print $o $r; close $o; sleep 300'",
            dir = dir.display()
        )
    }) else {
        return;
    };
    let outcome = attack_outcome(&attack.scratch);
    let claim = Caller::principal("lead", Peer::of_pid(attack.attacker)).unwrap();
    // Acting as A, C could stop (and answer for) itself; acting as itself it may not.
    let stopped = attack.supervisor.stop(&claim, attack.c.id);
    assert_eq!(outcome, "refused", "C's process joined A's process group");
    assert_eq!(stopped.map_err(|error| error.code()), Err("not_owner"));
    let _ = attack.a;
}

#[test]
fn a_child_that_leaves_for_a_session_of_its_own_is_still_its_session() {
    let Some(attack) = lateral(|dir| {
        format!(
            "-MPOSIX -e 'my $r = POSIX::setsid() > 0 ? q{{left}} : q{{stayed}}; \
             open(my $o, q{{>}}, q{{{dir}/outcome}}) or die; print $o $r; close $o; sleep 300'",
            dir = dir.display()
        )
    }) else {
        return;
    };
    assert_eq!(attack_outcome(&attack.scratch), "left");
    let refused = attack.supervisor.operator(Peer::of_pid(attack.attacker));
    assert_eq!(refused.err().map(|error| error.code()), Some("caller_not_admitted"));
    let claim = Caller::principal("lead", Peer::of_pid(attack.attacker)).unwrap();
    // As the configured launcher it would launch at depth 1 and own A; as C it sits at depth 2.
    let launched = attack
        .supervisor
        .launch(&claim, HarnessKind::ClaudeCode, &request())
        .unwrap();
    assert_eq!(
        launched.depth, 3,
        "the escaped process launched as the configured launcher"
    );
    assert_eq!(
        attack
            .supervisor
            .stop(&claim, attack.a.id)
            .map_err(|error| error.code()),
        Err("not_owner")
    );
}

#[test]
fn a_process_below_the_supervisor_in_no_session_is_no_launcher() {
    let scratch = Scratch::new();
    let config = scratch.config(HarnessKind::ClaudeCode, &scratch.long());
    let (supervisor, _) = start(policy(&[HarnessKind::ClaudeCode]), vec![config]);
    // A process the supervisor's process started outside any session, as a leftover of a
    // finished session would be.
    let mut stray = std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let claim = Caller::principal("lead", Peer::of_pid(stray.id())).unwrap();
    let refused = supervisor.launch(&claim, HarnessKind::ClaudeCode, &request());
    let _ = stray.kill();
    let _ = stray.wait();
    assert_eq!(
        refused.map(|launched| launched.depth).map_err(|error| error.code()),
        Err("caller_not_admitted")
    );
}

#[test]
fn only_a_peer_outside_the_supervisors_tree_is_made_the_operator() {
    let scratch = Scratch::new();
    let config = scratch.config(HarnessKind::ClaudeCode, &scratch.long());
    let (supervisor, audit) = start(policy(&[HarnessKind::ClaudeCode]), vec![config]);
    let launched = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request()).unwrap();
    // A launched agent running `basemind agent approve` from its shell: a process of the session.
    let refused = supervisor.operator(inside(&supervisor, launched.id));
    assert_eq!(refused.err().map(|error| error.code()), Some("caller_not_admitted"));
    // A process this process started outside any session.
    let mut stray = std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let refused = supervisor.operator(Peer::of_pid(stray.id()));
    let _ = stray.kill();
    let _ = stray.wait();
    assert_eq!(refused.err().map(|error| error.code()), Some("caller_not_admitted"));
    assert_eq!(
        outcomes(&audit, AuditEvent::Refusal),
        ["caller_not_admitted", "caller_not_admitted"]
    );
    // The operator's own process, outside the tree.
    let operator = supervisor.operator(outside()).unwrap();
    supervisor.stop(&operator, launched.id).unwrap();
    assert_eq!(finished(&supervisor, launched.id), FinishStatus::Stopped);
}

/// An OpenCode session whose stand-in starts `perl <attack>` in the background, then completes
/// its turn and ends. Returns the attacker as it started.
fn detached_attack(
    scratch: &Scratch,
    supervisor_configs: &mut Vec<LaunchConfig>,
    perl: &Path,
    attack: &str,
) -> PathBuf {
    let pidfile = scratch.path.join("attacker.pid");
    let body = format!(
        "'{perl}' {attack} &\necho $! > '{pidfile}'\n{HANDSHAKE}sleep 1\n{RESULT}cat > /dev/null\n",
        perl = perl.display(),
        pidfile = pidfile.display(),
    );
    supervisor_configs.push(scratch.config(OPENCODE, &body));
    pidfile
}

/// Kills the attacker `started` at the end of a test, only while it is still that process.
struct Reclaim(Peer);

impl Drop for Reclaim {
    fn drop(&mut self) {
        let pid = self.0.id.pid;
        if crate::procinfo::info(pid).is_some_and(|now| now.start == self.0.id.start) {
            // SAFETY: the test started this process, and its start time was checked just now.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

#[test]
fn a_detached_process_that_joins_a_sibling_sessions_group_does_not_take_it_down() {
    let Some(perl) = perl() else {
        return;
    };
    let scratch = Scratch::new();
    let attack = format!(
        "-e 'open(my $f, q{{<}}, q{{{dir}/b.pid}}) or die; my $b = <$f>; chomp $b; \
         my $r = setpgrp(0, $b) ? q{{joined}} : q{{refused}}; \
         open(my $o, q{{>}}, q{{{dir}/outcome}}) or die; print $o $r; close $o; sleep 300'",
        dir = scratch.path.display()
    );
    let mut configs = vec![scratch.config(HarnessKind::ClaudeCode, &scratch.long())];
    let pidfile = detached_attack(&scratch, &mut configs, &perl, &attack);
    let (supervisor, _) = start(policy(&[HarnessKind::ClaudeCode, OPENCODE]), configs);
    let sibling = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request()).unwrap();
    let sibling_pid = pid_of(&supervisor, sibling.id);
    std::fs::write(scratch.path.join("b.pid"), sibling_pid.to_string()).unwrap();
    let second = Caller::principal("second", outside()).unwrap();
    let launched = supervisor.launch(&second, OPENCODE, &request()).unwrap();
    let _reclaim = Reclaim(Peer::of_pid(u32::try_from(recorded_pid(&pidfile)).unwrap()));
    let outcome = attack_outcome(&scratch);
    assert_eq!(finished(&supervisor, launched.id), FinishStatus::Completed);
    assert!(alive(sibling_pid), "the reaper killed a sibling session ({outcome})");
    assert_eq!(supervisor.status(sibling.id).unwrap().state, SessionState::Running);
    assert_eq!(outcome, "refused");
}

#[test]
fn a_detached_process_that_joins_the_launchers_group_is_still_reaped() {
    let Some(perl) = perl() else {
        return;
    };
    let scratch = Scratch::new();
    // SAFETY: getpgrp has no preconditions.
    let own_group = unsafe { libc::getpgrp() };
    let attack = format!(
        "-e 'my $r = setpgrp(0, {own_group}) ? q{{joined}} : q{{refused}}; \
         open(my $o, q{{>}}, q{{{dir}/outcome}}) or die; print $o $r; close $o; sleep 300'",
        dir = scratch.path.display()
    );
    let mut configs = Vec::new();
    let pidfile = detached_attack(&scratch, &mut configs, &perl, &attack);
    let (supervisor, _) = start(policy(&[OPENCODE]), configs);
    let launched = supervisor.launch(&lead(), OPENCODE, &request()).unwrap();
    let attacker = Peer::of_pid(u32::try_from(recorded_pid(&pidfile)).unwrap());
    let _reclaim = Reclaim(attacker);
    let outcome = attack_outcome(&scratch);
    assert_eq!(finished(&supervisor, launched.id), FinishStatus::Completed);
    let survived =
        crate::procinfo::info(attacker.id.pid).is_some_and(|now| now.start == attacker.id.start && !now.zombie);
    assert!(
        !survived,
        "a process in the launcher's group outlived its session ({outcome})"
    );
}

/// An adapter whose launch panics, as a bug in an adapter would.
struct Panics(HarnessKind);

impl Adapter for Panics {
    fn supports(&self, kind: HarnessKind) -> bool {
        kind == self.0
    }

    fn plan(&self, config: &LaunchConfig, request: &LaunchRequest) -> Result<CommandPlan, LaunchError> {
        ClaudeCodeAdapter.plan(&as_claude(config), request)
    }

    fn launch(&self, _: &LaunchConfig, _: &LaunchRequest) -> Result<Session, LaunchError> {
        panic!("the adapter failed");
    }
}

#[test]
fn a_panicking_adapter_releases_its_slot() {
    let scratch = Scratch::new();
    let codex = HarnessKind::Codex;
    let mut registry = registry();
    registry.register(Panics(codex));
    let audit = Arc::new(MemoryAuditSink::default());
    let supervisor = Supervisor::new(
        LaunchPolicy::new(PolicySpec {
            max_concurrent: 1,
            max_children: 1,
            ..policy(&[HarnessKind::ClaudeCode, codex])
        })
        .unwrap(),
        vec![
            scratch.config(HarnessKind::ClaudeCode, &scratch.long()),
            scratch.config(codex, ""),
        ],
        registry,
        audit,
    )
    .unwrap();
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = supervisor.launch(&lead(), codex, &request());
    }));
    assert!(panicked.is_err());
    assert!(
        supervisor.list().iter().all(|status| !status.state.live()),
        "a slot leaked"
    );
    supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request()).unwrap();
}

/// Panics on the first event it sees.
struct PanickingObserver;

impl SessionObserver for PanickingObserver {
    fn event(&self, _: SessionId, _: &Event) {
        panic!("the observer failed");
    }
}

#[test]
fn a_panicking_observer_ends_its_session_instead_of_wedging_it() {
    let scratch = Scratch::new();
    // An approval request is the first event the observer sees, while the session runs.
    let body = format!(
        r#"{HANDSHAKE}printf '%s\n' '{{"type":"control_request","request_id":"req-1","request":{{"subtype":"can_use_tool","tool_name":"Bash","input":{{"command":"true"}}}}}}'
cat > /dev/null
"#
    );
    let config = scratch.config(HarnessKind::ClaudeCode, &body);
    let (supervisor, _) = start(policy(&[HarnessKind::ClaudeCode]), vec![config]);
    let supervisor = supervisor.with_observer(Arc::new(PanickingObserver));
    let launched = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request()).unwrap();
    let pid = pid_of(&supervisor, launched.id);
    let state = supervisor.wait(launched.id, Duration::from_secs(10));
    assert!(matches!(state, Some(SessionState::Finished(_))), "{state:?}");
    assert!(!alive(pid));
}

#[test]
fn a_stopping_session_cannot_launch_and_a_stop_reaches_its_descendants() {
    let scratch = Scratch::new();
    // Ignores the end of its input and SIGTERM, so a stop takes its whole grace.
    let lingering = format!(
        "trap '' TERM\nenv > '{}/'\"$BASEMIND_AGENT_ID\".env\n{HANDSHAKE}cat > /dev/null\nwhile :; do sleep 0.1; done\n",
        scratch.path.display()
    );
    let mut config = scratch.config(HarnessKind::ClaudeCode, &lingering);
    config.limits.stop_grace = Duration::from_secs(5);
    let (supervisor, _) = start(
        PolicySpec {
            max_depth: 3,
            ..policy(&[HarnessKind::ClaudeCode])
        },
        vec![config],
    );
    let parent = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request()).unwrap();
    let token = scratch.env_of(&parent.agent)["BASEMIND_LAUNCH_TOKEN"].clone();
    let as_parent = Caller::launched(&token, inside(&supervisor, parent.id));
    let child = supervisor
        .launch(&as_parent, HarnessKind::ClaudeCode, &request())
        .unwrap();
    supervisor.stop(&lead(), parent.id).unwrap();
    assert_eq!(supervisor.status(parent.id).unwrap().state, SessionState::Stopping);
    let refused = supervisor.launch(&as_parent, HarnessKind::ClaudeCode, &request());
    assert_eq!(
        refused.map(|launched| launched.depth).map_err(|error| error.code()),
        Err("caller_not_admitted")
    );
    assert_eq!(
        finished(&supervisor, child.id),
        FinishStatus::Stopped,
        "the child outlived its parent's stop"
    );
    assert_eq!(finished(&supervisor, parent.id), FinishStatus::Stopped);
}

/// An audit sink as slow as a disk under load, that fails the event kinds it is told to.
#[derive(Default)]
struct SlowSink {
    delay: Duration,
    failing: Vec<AuditEvent>,
    records: Mutex<Vec<AuditRecord>>,
}

impl AuditSink for SlowSink {
    fn record(&self, record: &AuditRecord) -> std::io::Result<()> {
        std::thread::sleep(self.delay);
        if self.failing.contains(&record.event) {
            return Err(std::io::Error::other("the disk is full"));
        }
        self.records.lock().unwrap().push(record.clone());
        Ok(())
    }
}

#[test]
fn a_slow_audit_write_does_not_hold_the_session_table() {
    let scratch = Scratch::new();
    let sink = Arc::new(SlowSink {
        delay: Duration::from_millis(600),
        ..SlowSink::default()
    });
    let supervisor = Arc::new(
        Supervisor::new(
            LaunchPolicy::new(policy(&[HarnessKind::ClaudeCode])).unwrap(),
            vec![scratch.config(HarnessKind::ClaudeCode, &scratch.long())],
            registry(),
            sink,
        )
        .unwrap(),
    );
    let flooding = std::thread::spawn({
        let supervisor = supervisor.clone();
        move || {
            let stranger = Caller::principal("stranger", outside()).unwrap();
            let _ = supervisor.launch(&stranger, HarnessKind::ClaudeCode, &request());
        }
    });
    std::thread::sleep(Duration::from_millis(100));
    let started = Instant::now();
    let _ = supervisor.list();
    let waited = started.elapsed();
    flooding.join().unwrap();
    assert!(
        waited < Duration::from_millis(300),
        "listing waited {waited:?} for an audit write"
    );
}

#[test]
fn a_flood_of_refusals_is_coalesced() {
    let scratch = Scratch::new();
    let config = scratch.config(HarnessKind::ClaudeCode, &scratch.long());
    let (supervisor, audit) = start(policy(&[HarnessKind::ClaudeCode]), vec![config]);
    let stranger = Caller::principal("stranger", outside()).unwrap();
    for _ in 0..200 {
        let refused = supervisor.launch(&stranger, HarnessKind::ClaudeCode, &request());
        assert_eq!(refused.err().map(|error| error.code()), Some("caller_not_admitted"));
    }
    let written = outcomes(&audit, AuditEvent::Refusal).len();
    assert!(written <= 20, "{written} refusal records for one caller's flood");
}

#[test]
fn an_allow_whose_record_cannot_be_written_is_not_delivered() {
    let scratch = Scratch::new();
    let body = format!(
        r#"{HANDSHAKE}printf '%s\n' '{{"type":"control_request","request_id":"req-1","request":{{"subtype":"can_use_tool","tool_name":"Bash","input":{{"command":"true"}}}}}}'
cat > /dev/null
"#
    );
    let mut config = scratch.config(HarnessKind::ClaudeCode, &body);
    config.approval_policy = crate::test_support::always(crate::spec::ApprovalVerdict::Ask);
    let sink = Arc::new(SlowSink {
        failing: vec![AuditEvent::ApprovalAnswer],
        ..SlowSink::default()
    });
    let spec = PolicySpec {
        flavors: vec![FlavorPolicy {
            approval_policy: true,
            ..FlavorPolicy::supervised(HarnessKind::ClaudeCode)
        }],
        ..policy(&[])
    };
    let recorder = Arc::new(Recorder::default());
    let supervisor = Supervisor::new(LaunchPolicy::new(spec).unwrap(), vec![config], registry(), sink.clone())
        .unwrap()
        .with_observer(recorder.clone());
    let launched = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let approval = loop {
        if let Some(approval) = recorder.approval() {
            break approval;
        }
        assert!(Instant::now() < deadline, "the observer saw no approval request");
        std::thread::sleep(Duration::from_millis(20));
    };
    let answered = supervisor.answer(&lead(), launched.id, &approval, true);
    supervisor.stop(&lead(), launched.id).unwrap();
    assert_eq!(finished(&supervisor, launched.id), FinishStatus::Stopped);
    let allowed = sink
        .records
        .lock()
        .unwrap()
        .iter()
        .any(|record| record.approval.as_ref().is_some_and(|approval| approval.allowed));
    assert!(!allowed, "an allow was delivered without its audit record");
    assert_eq!(answered.err().map(|error| error.code()), Some("audit_failed"));
}
