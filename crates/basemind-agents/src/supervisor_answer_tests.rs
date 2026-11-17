//! Who may answer or stop a session, and what an answer spends and records.

use super::attack_tests::{approvals_body, consenting, replies};
use super::tests::*;
use super::*;
use crate::audit::MemoryAuditSink;
use crate::event::FinishStatus;
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

#[test]
fn an_allow_that_did_not_reach_the_harness_is_kept_on_the_record() {
    let scratch = Scratch::new();
    // Asks once, then exits: the session ends with the request pending.
    let body = format!(
        r#"{HANDSHAKE}printf '%s\n' '{{"type":"control_request","request_id":"req-1","request":{{"subtype":"can_use_tool","tool_name":"Bash","input":{{"command":"true"}}}}}}'
"#
    );
    let mut config = scratch.config(HarnessKind::ClaudeCode, &body);
    config.approval_policy = crate::test_support::always(ApprovalVerdict::Ask);
    let (supervisor, audit) = start(consenting(), vec![config]);
    let gate = Arc::new(Gate::default());
    let supervisor = supervisor.with_observer(gate.clone());
    let launched = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request()).unwrap();
    let approval = pending(&gate.seen, 1);
    // The session withdrew the request as it ended; its thread is held before the supervisor
    // hears so, and the session refuses controls once it is over.
    let handle = supervisor.inner.table().sessions[&launched.id.0]
        .handle
        .clone()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !handle.finished() {
        assert!(Instant::now() < deadline, "the session never ended");
        std::thread::sleep(Duration::from_millis(10));
    }
    let answered = supervisor.answer(&supervisor.operator().unwrap(), launched.id, &approval, true);
    assert_eq!(answered.err().map(|error| error.code()), Some("session_closed"));
    gate.open();
    assert_eq!(answers(&audit), ["allowed", "allow_undelivered"]);
    let undelivered = audit
        .records()
        .into_iter()
        .find(|record| record.outcome == "allow_undelivered")
        .unwrap();
    assert!(crate::audit::kept_for(&undelivered), "the correction may be dropped");
    assert_eq!(undelivered.suppressed, None);
    assert!(undelivered.approval.is_some_and(|approval| !approval.allowed));
}

#[test]
fn the_top_of_a_tree_stops_a_grandchild_whose_parent_was_reaped() {
    let scratch = Scratch::new();
    let dir = scratch.path.display();
    // Records its environment, then runs until its own release file appears.
    let body = format!(
        "env > '{dir}/'\"$BASEMIND_AGENT_ID\".env\n{HANDSHAKE}while [ ! -e '{dir}/'\"$BASEMIND_AGENT_ID\".release ]; do sleep 0.05; done\n{RESULT}"
    );
    let (supervisor, _) = start(
        PolicySpec {
            max_depth: 2,
            ..policy(&[HarnessKind::ClaudeCode])
        },
        vec![scratch.config(HarnessKind::ClaudeCode, &body)],
    );
    let parent = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request()).unwrap();
    let token = scratch.env_of(&parent.agent)["BASEMIND_LAUNCH_TOKEN"].clone();
    let child = supervisor
        .launch(
            &Caller::launched(&token, inside(&supervisor, parent.id)),
            HarnessKind::ClaudeCode,
            &request(),
        )
        .unwrap();
    let _ = scratch.env_of(&child.agent);
    // The parent finishes on its own, which stops nothing below it, and is forgotten.
    std::fs::write(scratch.path.join(format!("{}.release", parent.agent)), "").unwrap();
    assert_eq!(finished(&supervisor, parent.id), FinishStatus::Completed);
    assert_eq!(supervisor.reap().len(), 1);
    assert_eq!(supervisor.status(child.id).unwrap().state, SessionState::Running);
    // Another configured launcher still may not; the one at the top of the tree may.
    let second = Caller::principal("second", own()).unwrap();
    assert_eq!(supervisor.stop(&second, child.id).unwrap_err().code(), "not_owner");
    supervisor.stop(&lead(), child.id).unwrap();
    assert_eq!(finished(&supervisor, child.id), FinishStatus::Stopped);
}

/// Holds the session's thread inside the observer on every approval request until it is opened,
/// as an embedding that answers from the observer would.
#[derive(Default)]
struct HoldOnRequest {
    gate: Gate,
}

impl SessionObserver for HoldOnRequest {
    fn event(&self, session: SessionId, event: &crate::event::Event) {
        self.gate.seen.event(session, event);
        if matches!(event, crate::event::Event::ApprovalRequest(_)) {
            let open = self.gate.open.lock().unwrap();
            let _ = self
                .gate
                .opened
                .wait_timeout_while(open, Duration::from_secs(20), |open| !*open)
                .unwrap();
        }
    }
}

#[test]
fn a_request_is_pending_before_the_observer_hears_of_it() {
    let scratch = Scratch::new();
    let mut config = scratch.config(HarnessKind::ClaudeCode, &approvals_body(&scratch, 1));
    config.approval_policy = crate::test_support::always(ApprovalVerdict::Ask);
    let (supervisor, _audit) = start(consenting(), vec![config]);
    let observer = Arc::new(HoldOnRequest::default());
    let supervisor = supervisor.with_observer(observer.clone());
    let launched = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request()).unwrap();
    // The observer holds the request while it is answered: the supervisor already has it.
    let id = pending(&observer.gate.seen, 1);
    let answered = supervisor.answer(&lead(), launched.id, &id, false);
    observer.gate.open();
    answered.unwrap();
    await_replies(&scratch, 1);
    assert_eq!(replies(&scratch), (0, 1));
}
