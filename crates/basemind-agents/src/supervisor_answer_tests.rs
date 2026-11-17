//! Who may answer a session's approval request, and what an answer spends and records.

use super::attack_tests::{approvals_body, consenting, replies};
use super::tests::*;
use super::*;
use crate::audit::MemoryAuditSink;
use crate::policy::{FlavorPolicy, PolicySpec};
use crate::spec::ApprovalVerdict;

/// A supervisor whose Claude Code sessions ask `count` approvals, each left to a caller, and
/// the observer that sees their ids.
fn asking(scratch: &Scratch, spec: PolicySpec, count: usize) -> (Supervisor, Arc<MemoryAuditSink>, Arc<Recorder>) {
    let mut config = scratch.config(HarnessKind::ClaudeCode, &approvals_body(scratch, count));
    config.approval_policy = crate::test_support::always(ApprovalVerdict::Ask);
    let (supervisor, audit) = start(spec, vec![config]);
    let recorder = Arc::new(Recorder::default());
    (supervisor.with_observer(recorder.clone()), audit, recorder)
}

/// The id of the `count`th approval request the observer saw.
fn pending(recorder: &Recorder, count: usize) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let ids = recorder.approvals();
        if ids.len() >= count {
            return ids[count - 1].clone();
        }
        assert!(Instant::now() < deadline, "no approval request {count}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Waits until the stand-in of [`approvals_body`] has read `count` answers.
fn await_replies(scratch: &Scratch, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while {
        let (allowed, denied) = replies(scratch);
        allowed + denied < count
    } {
        assert!(
            Instant::now() < deadline,
            "the stand-in read fewer than {count} answers"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The outcomes of the answer records.
fn answers(audit: &MemoryAuditSink) -> Vec<&'static str> {
    outcomes(audit, AuditEvent::ApprovalAnswer)
}

#[test]
fn a_launcher_may_deny_but_only_the_operator_allows() {
    let scratch = Scratch::new();
    let (supervisor, audit, recorder) = asking(&scratch, consenting(), 2);
    let launched = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request()).unwrap();
    let first = pending(&recorder, 1);
    let refused = supervisor.answer(&lead(), launched.id, &first, true);
    assert_eq!(refused.err().map(|error| error.code()), Some("launcher_may_not_allow"));
    // Nothing reached the harness, and the request is still pending: the operator allows it.
    assert_eq!(replies(&scratch), (0, 0));
    supervisor
        .answer(&supervisor.operator().unwrap(), launched.id, &first, true)
        .unwrap();
    // The launcher may deny.
    supervisor
        .answer(&lead(), launched.id, &pending(&recorder, 2), false)
        .unwrap();
    await_replies(&scratch, 2);
    assert_eq!(replies(&scratch), (1, 1), "(allowed, denied) as the harness read them");
    assert_eq!(answers(&audit), ["launcher_may_not_allow", "allowed", "denied"]);
    let allowed = audit
        .records()
        .into_iter()
        .find(|record| record.outcome == "allowed")
        .unwrap();
    assert_eq!(allowed.caller.as_deref(), Some("operator"));
}

#[test]
fn a_flavor_that_lets_launchers_allow_takes_the_launchers_allow() {
    let scratch = Scratch::new();
    let spec = PolicySpec {
        flavors: vec![FlavorPolicy {
            approval_policy: true,
            launcher_may_allow: true,
            ..FlavorPolicy::supervised(HarnessKind::ClaudeCode)
        }],
        ..consenting()
    };
    let (supervisor, audit, recorder) = asking(&scratch, spec, 1);
    let launched = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request()).unwrap();
    supervisor
        .answer(&lead(), launched.id, &pending(&recorder, 1), true)
        .unwrap();
    await_replies(&scratch, 1);
    assert_eq!(replies(&scratch), (1, 0));
    assert_eq!(answers(&audit), ["allowed"]);
}

/// Records the approval requests it sees, and holds the session's thread on each settlement
/// until it is opened: a settled request stays pending in the supervisor meanwhile.
#[derive(Default)]
struct Gate {
    seen: Recorder,
    open: std::sync::Mutex<bool>,
    opened: std::sync::Condvar,
}

impl SessionObserver for Gate {
    fn event(&self, session: SessionId, event: &crate::event::Event) {
        self.seen.event(session, event);
        if matches!(event, crate::event::Event::ApprovalResolved { .. }) {
            let open = self.open.lock().unwrap();
            let _ = self
                .opened
                .wait_timeout_while(open, Duration::from_secs(20), |open| !*open)
                .unwrap();
        }
    }
}

impl Gate {
    fn open(&self) {
        *self.open.lock().unwrap() = true;
        self.opened.notify_all();
    }
}

#[test]
fn a_pending_request_is_allowed_spent_and_recorded_once() {
    let scratch = Scratch::new();
    let mut config = scratch.config(HarnessKind::ClaudeCode, &approvals_body(&scratch, 2));
    config.approval_policy = crate::test_support::always(ApprovalVerdict::Ask);
    let spec = PolicySpec {
        budget: crate::policy::Budget {
            max_allows: 2,
            ..consenting().budget
        },
        ..consenting()
    };
    let (supervisor, audit) = start(spec, vec![config]);
    let gate = Arc::new(Gate::default());
    let supervisor = supervisor.with_observer(gate.clone());
    let launched = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request()).unwrap();
    let operator = supervisor.operator().unwrap();
    let first = pending(&gate.seen, 1);
    supervisor.answer(&operator, launched.id, &first, true).unwrap();
    // The session settled it, and its thread is held before the supervisor hears so: the
    // request is still pending there, already answered.
    let again = supervisor.answer(&operator, launched.id, &first, true);
    assert_eq!(again.err().map(|error| error.code()), Some("unknown_approval"));
    gate.open();
    // The repeated allow spent nothing: the budget's second allow is still there.
    supervisor
        .answer(&operator, launched.id, &pending(&gate.seen, 2), true)
        .unwrap();
    await_replies(&scratch, 2);
    assert_eq!(replies(&scratch), (2, 0), "(allowed, denied) as the harness read them");
    assert_eq!(answers(&audit), ["allowed", "unknown_approval", "allowed"]);
}
