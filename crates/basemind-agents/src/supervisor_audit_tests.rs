//! What the audit log has to hold over a supervisor's life, and what happens when it cannot:
//! the allow budget, refusal coalescing, the reserve and exhaustion.

use super::attack_tests::{approvals_body, consenting, replies};
use super::tests::*;
use super::*;
use crate::policy::{Budget, PolicySpec};
use crate::spec::ApprovalVerdict;

/// Waits until the stand-in of [`approvals_body`] has read `count` answers.
fn await_replies(scratch: &Scratch, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let (allowed, denied) = replies(scratch);
        if allowed + denied >= count {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the stand-in read {} of {count} answers",
            allowed + denied
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn with_allows(max_allows: u32) -> PolicySpec {
    let spec = consenting();
    PolicySpec {
        budget: Budget {
            max_allows,
            ..spec.budget
        },
        ..spec
    }
}

#[test]
fn the_allow_budget_is_shared_across_sessions() {
    let scratch = Scratch::new();
    let mut config = scratch.config(HarnessKind::ClaudeCode, &approvals_body(&scratch, 2));
    config.approval_policy = crate::test_support::always(ApprovalVerdict::Allow);
    let (supervisor, audit) = start(with_allows(3), vec![config]);
    let first = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request()).unwrap();
    await_replies(&scratch, 2);
    let second = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request()).unwrap();
    await_replies(&scratch, 4);
    // Two sessions, two allows each asked: the third is the supervisor's last.
    assert_eq!(
        replies(&scratch),
        (3, 1),
        "(allowed, denied) as the harnesses read them"
    );
    let allows = outcomes(&audit, AuditEvent::Approval)
        .iter()
        .filter(|outcome| **outcome == "allowed")
        .count();
    assert_eq!(allows, 3);
    let _ = (first, second);
}

#[test]
fn a_caller_allow_spends_the_allow_budget() {
    let scratch = Scratch::new();
    let mut config = scratch.config(HarnessKind::ClaudeCode, &approvals_body(&scratch, 2));
    config.approval_policy = crate::test_support::always(ApprovalVerdict::Ask);
    let recorder = Arc::new(Recorder::default());
    let (supervisor, audit) = start(with_allows(1), vec![config]);
    let supervisor = supervisor.with_observer(recorder.clone());
    let launched = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request()).unwrap();
    let pending = |count: usize| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let ids = recorder.approvals();
            if ids.len() >= count {
                return ids[count - 1].clone();
            }
            assert!(Instant::now() < deadline, "no approval request {count}");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    supervisor.answer(&lead(), launched.id, &pending(1), true).unwrap();
    let refused = supervisor.answer(&lead(), launched.id, &pending(2), true);
    assert_eq!(refused.err().map(|error| error.code()), Some("allows_exhausted"));
    supervisor.answer(&lead(), launched.id, &pending(2), false).unwrap();
    await_replies(&scratch, 2);
    assert_eq!(replies(&scratch), (1, 1));
    let answered: Vec<_> = audit
        .records()
        .iter()
        .filter(|record| record.event == AuditEvent::ApprovalAnswer)
        .map(|record| record.outcome)
        .collect();
    assert_eq!(answered, ["allowed", "allows_exhausted", "denied"]);
}

#[test]
fn a_caller_cannot_spread_its_refusals_over_outcomes() {
    let scratch = Scratch::new();
    let config = scratch.config(HarnessKind::ClaudeCode, &scratch.long());
    let (supervisor, audit) = start(policy(&[HarnessKind::ClaudeCode]), vec![config]);
    let nowhere = SessionId::from_raw(999);
    for _ in 0..50 {
        let refused = [
            supervisor
                .launch(&lead(), HarnessKind::ClaudeCode, &LaunchRequest::new("/context"))
                .map(|_| ()),
            supervisor.launch(&lead(), HarnessKind::Codex, &request()).map(|_| ()),
            supervisor.stop(&lead(), nowhere),
            supervisor.answer(&lead(), nowhere, "never-issued", false),
        ];
        let codes: Vec<_> = refused
            .iter()
            .map(|refused| refused.as_ref().err().map(SupervisorError::code))
            .collect();
        assert_eq!(
            codes,
            [
                Some("slash_command_prompt"),
                Some("flavor_not_allowed"),
                Some("unknown_session"),
                Some("unknown_session")
            ]
        );
    }
    let written = audit
        .records()
        .iter()
        .filter(|record| record.caller.as_deref() == Some("lead") && record.event != AuditEvent::Registration)
        .count();
    assert_eq!(
        written,
        crate::refusals::BURST as usize,
        "200 refusals of one caller over four outcomes"
    );
    drop(supervisor);
    let suppressed: u64 = audit.records().iter().filter_map(|record| record.suppressed).sum();
    assert_eq!(suppressed, 200 - u64::from(crate::refusals::BURST));
}

#[test]
fn a_budget_whose_records_do_not_fit_the_reserve_is_refused() {
    let scratch = Scratch::new();
    let configs = || vec![scratch.config(HarnessKind::ClaudeCode, &scratch.long())];
    let spec = policy(&[HarnessKind::ClaudeCode]);
    let needed = crate::supervisor_reserve::demand(&LaunchPolicy::new(spec.clone()).unwrap(), &configs());
    // A log whose reserve (a quarter of the cap) holds one supervisor's life, not two.
    let sink = Arc::new(
        crate::audit::FileAuditSink::open(scratch.path.join("audit.jsonl"))
            .unwrap()
            .with_max_bytes(4 * needed + 4 * needed / 2),
    );
    let first = Supervisor::new(
        LaunchPolicy::new(spec.clone()).unwrap(),
        configs(),
        registry(),
        sink.clone(),
    );
    assert!(first.is_ok(), "{:?}", first.err());
    let second = Supervisor::new(LaunchPolicy::new(spec.clone()).unwrap(), configs(), registry(), sink);
    assert_eq!(second.err(), Some(PolicyError::AuditReserve { needed }));
    // A budget too large for a log of its own is refused the same way.
    let small = Arc::new(
        crate::audit::FileAuditSink::open(scratch.path.join("small.jsonl"))
            .unwrap()
            .with_max_bytes(needed),
    );
    let refused = Supervisor::new(LaunchPolicy::new(spec).unwrap(), configs(), registry(), small);
    assert_eq!(refused.err(), Some(PolicyError::AuditReserve { needed }));
}

#[test]
fn registrations_are_bounded_over_the_supervisors_life() {
    let scratch = Scratch::new();
    let config = scratch.config(HarnessKind::ClaudeCode, &scratch.long());
    let (supervisor, _) = start(policy(&[HarnessKind::ClaudeCode]), vec![config]);
    // `start` registered three launchers and the operator.
    let mut made = 4;
    while made < crate::supervisor_reserve::MAX_REGISTRATIONS {
        assert!(supervisor.unregister_launcher("lead"));
        supervisor.register_launcher_in_process("lead").unwrap();
        made += 2;
    }
    assert!(
        supervisor.unregister_launcher("lead"),
        "a release past the limit was not made"
    );
    assert_eq!(
        supervisor
            .register_launcher_in_process("lead")
            .map_err(|error| error.code()),
        Err("registration_limit")
    );
}

#[test]
fn a_launch_that_failed_after_its_record_spends_the_budget() {
    let scratch = Scratch::new();
    let codex = HarnessKind::Codex;
    let mut registry = registry();
    registry.register(super::attack_tests::Panics(codex));
    let spec = PolicySpec {
        budget: Budget {
            max_launches: 1,
            ..policy(&[]).budget
        },
        ..policy(&[HarnessKind::ClaudeCode, codex])
    };
    let audit = Arc::new(crate::audit::MemoryAuditSink::default());
    let supervisor = registered(
        Supervisor::new(
            LaunchPolicy::new(spec).unwrap(),
            vec![
                scratch.config(HarnessKind::ClaudeCode, &scratch.long()),
                scratch.config(codex, ""),
            ],
            registry,
            audit.clone(),
        )
        .unwrap(),
    );
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = supervisor.launch(&lead(), codex, &request());
    }));
    assert!(panicked.is_err());
    // Its launch record went to the reserve, so the launch is not given back.
    let refused = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request());
    assert_eq!(refused.err().map(|error| error.code()), Some("budget_exhausted"));
    assert_eq!(outcomes(&audit, AuditEvent::Launch), ["accepted"]);
}

/// A log whose reserve is gone: every launch record fails, and each attempt is counted.
#[derive(Default)]
struct NoRoom {
    launches_tried: std::sync::atomic::AtomicUsize,
}

impl AuditSink for NoRoom {
    fn record(&self, record: &AuditRecord) -> std::io::Result<()> {
        if record.event == AuditEvent::Launch {
            self.launches_tried.fetch_add(1, Ordering::SeqCst);
            return Err(std::io::Error::other("the audit log is full"));
        }
        Ok(())
    }
}

#[test]
fn an_exhausted_reserve_refuses_every_launch_and_says_so_outside_the_log() {
    let scratch = Scratch::new();
    let sink = Arc::new(NoRoom::default());
    let told = Arc::new(Told::default());
    let supervisor = registered(
        Supervisor::new(
            LaunchPolicy::new(policy(&[HarnessKind::ClaudeCode])).unwrap(),
            vec![scratch.config(HarnessKind::ClaudeCode, &scratch.long())],
            registry(),
            sink.clone(),
        )
        .unwrap(),
    )
    .with_observer(told.clone());
    assert!(!supervisor.audit_exhausted());
    let first = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request());
    assert_eq!(first.err().map(|error| error.code()), Some("audit_failed"));
    assert!(supervisor.audit_exhausted());
    assert_eq!(told.0.load(Ordering::SeqCst), 1, "the observer was not told");
    for _ in 0..3 {
        let refused = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request());
        assert_eq!(refused.err().map(|error| error.code()), Some("audit_exhausted"));
    }
    assert_eq!(
        sink.launches_tried.load(Ordering::SeqCst),
        1,
        "a launch was tried on an exhausted log"
    );
    assert!(supervisor.list().is_empty());
    assert_eq!(told.0.load(Ordering::SeqCst), 1, "the observer was told more than once");
}

/// Counts how often it was told the log is exhausted.
#[derive(Default)]
struct Told(std::sync::atomic::AtomicUsize);

impl SessionObserver for Told {
    fn event(&self, _: SessionId, _: &crate::event::Event) {}

    fn audit_exhausted(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
