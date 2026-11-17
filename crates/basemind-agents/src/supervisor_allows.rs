//! The allows a supervisor lets through, each of which is recorded: at most
//! [`MAX_SESSION_ALLOWS`] per session and the budget's `max_allows` over its life. Past either,
//! an allow the approval policy decides becomes a denial before the harness sees it, and one a
//! caller answers is refused, so the harness never gets an allow the log has no room for.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::event::ApprovalRequest;
use crate::spec::{ApprovalPolicy, ApprovalVerdict};

/// The most allows one session may be given, by its approval policy or a caller.
pub(crate) const MAX_SESSION_ALLOWS: u32 = 1024;

/// The allow counts one session spends from: its own and the supervisor's.
#[derive(Clone, Debug)]
pub(crate) struct Allowance {
    session: Arc<AtomicU32>,
    lifetime: Arc<AtomicU32>,
    max_allows: u32,
    /// The supervisor's audit log is exhausted: nothing more is allowed.
    exhausted: Arc<AtomicBool>,
}

impl Allowance {
    /// A new session's allowance, drawing on the supervisor's `lifetime` count up to
    /// `max_allows`, and none once `exhausted` is set.
    pub fn new(lifetime: Arc<AtomicU32>, max_allows: u32, exhausted: Arc<AtomicBool>) -> Self {
        Self {
            session: Arc::default(),
            lifetime,
            max_allows,
            exhausted,
        }
    }

    /// Spends one allow: whether one was left in both the session's and the supervisor's, and
    /// the audit log is not exhausted.
    pub fn take(&self) -> bool {
        if self.exhausted.load(Ordering::SeqCst) || !take(&self.session, MAX_SESSION_ALLOWS) {
            return false;
        }
        if take(&self.lifetime, self.max_allows) {
            return true;
        }
        self.session.fetch_sub(1, Ordering::SeqCst);
        false
    }
}

/// Increments `count` unless it is at `cap`.
fn take(count: &AtomicU32, cap: u32) -> bool {
    count
        .try_update(Ordering::SeqCst, Ordering::SeqCst, |now| (now < cap).then_some(now + 1))
        .is_ok()
}

/// A session's approval policy, with every allow spent from its [`Allowance`]: an allow that
/// finds none left is a denial. An allow is spent when the policy gives it, before the session
/// knows whether the harness can take it: an ACP agent that offers no single-use allow gets a
/// denial, and the allow stays spent.
pub(crate) struct Metered {
    pub policy: Arc<dyn ApprovalPolicy>,
    pub allowance: Allowance,
}

impl ApprovalPolicy for Metered {
    fn decide(&self, request: &ApprovalRequest) -> ApprovalVerdict {
        match self.policy.decide(request) {
            ApprovalVerdict::Allow if !self.allowance.take() => ApprovalVerdict::Deny,
            verdict => verdict,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_allowance_stops_at_the_session_cap_and_the_lifetime_cap() {
        let lifetime = Arc::new(AtomicU32::new(0));
        let exhausted = Arc::new(AtomicBool::new(false));
        let first = Allowance::new(lifetime.clone(), MAX_SESSION_ALLOWS + 1, exhausted.clone());
        let granted = (0..MAX_SESSION_ALLOWS + 5).filter(|_| first.take()).count();
        assert_eq!(granted, MAX_SESSION_ALLOWS as usize);
        let second = Allowance::new(lifetime.clone(), MAX_SESSION_ALLOWS + 1, exhausted.clone());
        assert!(second.take());
        assert!(!second.take(), "the lifetime cap was passed");
        // A refusal by the lifetime cap gives the session's count back.
        assert_eq!(second.session.load(Ordering::SeqCst), 1);
        assert_eq!(lifetime.load(Ordering::SeqCst), MAX_SESSION_ALLOWS + 1);
        // An exhausted log allows nothing, whatever is left.
        let third = Allowance::new(Arc::default(), 10, exhausted.clone());
        exhausted.store(true, Ordering::SeqCst);
        assert!(!third.take());
    }
}
