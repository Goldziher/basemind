//! The permission engine: policy evaluation plus a session remember-cache.
//!
//! [`PermissionEngine::evaluate`] resolves a [`PermissionClaim`] to a [`Decision`] by first
//! consulting the in-memory remember-cache (populated by "allow for session") and then the
//! [`RuleSet`] (last-match-wins, deny-by-default). The turn-loop turns an [`Decision::Ask`] into a
//! `PermissionRequested` event and suspends until the UI replies; that suspend lives in the loop
//! because it owns the command channel. This module owns the policy, the cache, and request ids.

mod rule;

pub use rule::{ClaimKind, PermissionClaim, Rule, RuleAction, RuleSet};

use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// The resolved decision for a claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Proceed without asking.
    Allow,
    /// Reject the claim.
    Deny,
    /// Ask the user (the turn-loop will emit `PermissionRequested`).
    Ask,
}

/// Owns the permission policy for one session.
pub struct PermissionEngine {
    rules: RuleSet,
    remembered: Mutex<HashSet<String>>,
    next_req_id: AtomicU64,
}

impl PermissionEngine {
    /// Build an engine from an explicit ruleset.
    pub fn new(rules: RuleSet) -> Self {
        Self {
            rules,
            remembered: Mutex::new(HashSet::new()),
            next_req_id: AtomicU64::new(1),
        }
    }

    /// Build an engine with the default base ruleset (read-only auto-allowed, rest `Ask`).
    pub fn with_base() -> Self {
        Self::new(RuleSet::base())
    }

    /// Resolve a claim. The remember-cache wins over the ruleset; otherwise the ruleset decides,
    /// defaulting to `Ask` for anything unmatched.
    pub fn evaluate(&self, claim: &PermissionClaim) -> Decision {
        if self.is_remembered(claim) {
            return Decision::Allow;
        }
        match self.rules.eval(claim) {
            RuleAction::Allow => Decision::Allow,
            RuleAction::Deny => Decision::Deny,
            RuleAction::Ask => Decision::Ask,
        }
    }

    /// Remember a claim so future identical claims auto-allow for the rest of the session.
    pub fn remember(&self, claim: &PermissionClaim) {
        // Recover from a poisoned lock rather than panicking: a poisoned permission cache would ~keep
        // otherwise make every subsequent tool call panic and deny the rest of the session. ~keep
        self.remembered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(claim.signature());
    }

    fn is_remembered(&self, claim: &PermissionClaim) -> bool {
        self.remembered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(&claim.signature())
    }

    /// Allocate the next monotonic permission-request id.
    pub fn next_request_id(&self) -> u64 {
        self.next_req_id.fetch_add(1, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_allows_reads_and_asks_for_writes() {
        let engine = PermissionEngine::with_base();
        assert_eq!(engine.evaluate(&PermissionClaim::read("src/lib.rs")), Decision::Allow);
        assert_eq!(engine.evaluate(&PermissionClaim::write("src/lib.rs")), Decision::Ask);
    }

    #[test]
    fn remember_upgrades_a_specific_claim_to_allow() {
        let engine = PermissionEngine::with_base();
        let claim = PermissionClaim::write("src/lib.rs");
        assert_eq!(engine.evaluate(&claim), Decision::Ask);
        engine.remember(&claim);
        assert_eq!(engine.evaluate(&claim), Decision::Allow);
        // A different target is unaffected. ~keep
        assert_eq!(engine.evaluate(&PermissionClaim::write("src/other.rs")), Decision::Ask);
    }

    #[test]
    fn explicit_deny_rule_denies() {
        let mut rules = RuleSet::base();
        rules.push(Rule::new(ClaimKind::Exec, "rm*", RuleAction::Deny).unwrap());
        let engine = PermissionEngine::new(rules);
        assert_eq!(engine.evaluate(&PermissionClaim::exec("rm -rf /")), Decision::Deny);
        assert_eq!(engine.evaluate(&PermissionClaim::exec("ls")), Decision::Ask);
    }

    #[test]
    fn request_ids_are_monotonic() {
        let engine = PermissionEngine::with_base();
        assert_eq!(engine.next_request_id(), 1);
        assert_eq!(engine.next_request_id(), 2);
        assert_eq!(engine.next_request_id(), 3);
    }
}
