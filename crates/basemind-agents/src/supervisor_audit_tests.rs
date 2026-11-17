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
