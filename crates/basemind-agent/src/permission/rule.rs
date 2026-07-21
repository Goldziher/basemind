//! Permission claims and rules.
//!
//! A tool declares a [`PermissionClaim`] describing what it is about to do (read/write a path,
//! run a command, reach the network). A [`RuleSet`] resolves the claim to an action using
//! **last-match-wins** wildcard rules; an unmatched claim resolves to [`RuleAction::Ask`] — the
//! deny-by-default posture (omitting a rule never silently allows; cf. grok-build CWE-1188).

use globset::{Glob, GlobMatcher};
use serde::{Deserialize, Serialize};

/// The category of side effect a claim represents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimKind {
    /// Reading a file or querying the code map.
    Read,
    /// Mutating a file.
    Write,
    /// Executing a shell command.
    Exec,
    /// Reaching the network.
    Network,
    /// Posting to the multi-agent comms substrate.
    Comms,
}

impl ClaimKind {
    /// A short lowercase label used in events and rule config.
    pub fn as_str(self) -> &'static str {
        match self {
            ClaimKind::Read => "read",
            ClaimKind::Write => "write",
            ClaimKind::Exec => "exec",
            ClaimKind::Network => "network",
            ClaimKind::Comms => "comms",
        }
    }
}

/// What a tool is about to do: a [`ClaimKind`] plus a target (path, command, host).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionClaim {
    /// The category of side effect.
    pub kind: ClaimKind,
    /// The target the action applies to — matched against rule patterns.
    pub target: String,
}

impl PermissionClaim {
    /// A read-only claim (auto-allowed by the base ruleset).
    pub fn read(target: impl Into<String>) -> Self {
        Self::new(ClaimKind::Read, target)
    }

    /// A file-write claim.
    pub fn write(target: impl Into<String>) -> Self {
        Self::new(ClaimKind::Write, target)
    }

    /// A shell-exec claim.
    pub fn exec(target: impl Into<String>) -> Self {
        Self::new(ClaimKind::Exec, target)
    }

    /// A network claim.
    pub fn network(target: impl Into<String>) -> Self {
        Self::new(ClaimKind::Network, target)
    }

    /// A comms claim.
    pub fn comms(target: impl Into<String>) -> Self {
        Self::new(ClaimKind::Comms, target)
    }

    /// Construct a claim from parts.
    pub fn new(kind: ClaimKind, target: impl Into<String>) -> Self {
        Self {
            kind,
            target: target.into(),
        }
    }

    /// A stable signature used as the key in the session remember-cache.
    pub fn signature(&self) -> String {
        format!("{}:{}", self.kind.as_str(), self.target)
    }
}

/// What a rule does when it matches a claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleAction {
    /// Permit the claim.
    Allow,
    /// Reject the claim.
    Deny,
    /// Ask the user.
    Ask,
}

/// A single permission rule: a kind + glob pattern + action.
pub struct Rule {
    kind: ClaimKind,
    matcher: GlobMatcher,
    action: RuleAction,
}

impl Rule {
    /// Compile a rule from a glob pattern (e.g. `src/**`, `secret*`, `*`). Returns an error if
    /// the pattern is not a valid glob.
    pub fn new(kind: ClaimKind, pattern: &str, action: RuleAction) -> Result<Self, globset::Error> {
        Ok(Self {
            kind,
            matcher: Glob::new(pattern)?.compile_matcher(),
            action,
        })
    }

    fn matches(&self, claim: &PermissionClaim) -> bool {
        self.kind == claim.kind && self.matcher.is_match(&claim.target)
    }
}

/// An ordered set of rules resolved last-match-wins.
#[derive(Default)]
pub struct RuleSet {
    rules: Vec<Rule>,
}

impl RuleSet {
    /// An empty ruleset (every claim resolves to [`RuleAction::Ask`]).
    pub fn new() -> Self {
        Self::default()
    }

    /// The default base ruleset: read-only claims are auto-allowed; everything else falls through
    /// to `Ask`. Panics only on an internal pattern bug (the `*` glob always compiles).
    pub fn base() -> Self {
        let mut set = Self::new();
        set.push(Rule::new(ClaimKind::Read, "*", RuleAction::Allow).expect("`*` is a valid glob"));
        set
    }

    /// Append a rule (later rules win).
    pub fn push(&mut self, rule: Rule) {
        self.rules.push(rule);
    }

    /// Resolve a claim: the last matching rule wins; no match resolves to `Ask`.
    pub fn eval(&self, claim: &PermissionClaim) -> RuleAction {
        self.rules
            .iter()
            .rev()
            .find(|rule| rule.matches(claim))
            .map(|rule| rule.action)
            .unwrap_or(RuleAction::Ask)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(kind: ClaimKind, pat: &str, action: RuleAction) -> Rule {
        Rule::new(kind, pat, action).expect("valid glob")
    }

    #[test]
    fn base_allows_reads_and_asks_for_everything_else() {
        let set = RuleSet::base();
        assert_eq!(set.eval(&PermissionClaim::read("src/lib.rs")), RuleAction::Allow);
        assert_eq!(set.eval(&PermissionClaim::write("src/lib.rs")), RuleAction::Ask);
        assert_eq!(set.eval(&PermissionClaim::exec("ls")), RuleAction::Ask);
        assert_eq!(set.eval(&PermissionClaim::network("example.com")), RuleAction::Ask);
    }

    #[test]
    fn unmatched_claim_is_ask_by_default() {
        let set = RuleSet::new();
        assert_eq!(set.eval(&PermissionClaim::read("anything")), RuleAction::Ask);
    }

    #[test]
    fn last_match_wins() {
        let mut set = RuleSet::new();
        set.push(rule(ClaimKind::Exec, "*", RuleAction::Deny));
        set.push(rule(ClaimKind::Exec, "ls", RuleAction::Allow));
        assert_eq!(set.eval(&PermissionClaim::exec("ls")), RuleAction::Allow);
        assert_eq!(set.eval(&PermissionClaim::exec("rm")), RuleAction::Deny);
    }

    #[test]
    fn a_later_deny_overrides_an_earlier_allow() {
        let mut set = RuleSet::new();
        set.push(rule(ClaimKind::Write, "**", RuleAction::Allow));
        set.push(rule(ClaimKind::Write, "**/secret*", RuleAction::Deny));
        assert_eq!(set.eval(&PermissionClaim::write("src/secret.txt")), RuleAction::Deny);
        assert_eq!(set.eval(&PermissionClaim::write("src/main.rs")), RuleAction::Allow);
    }

    #[test]
    fn rules_are_kind_scoped() {
        let mut set = RuleSet::new();
        set.push(rule(ClaimKind::Write, "*", RuleAction::Allow));
        // A write rule must not satisfy an exec claim with the same target.
        assert_eq!(set.eval(&PermissionClaim::exec("build.sh")), RuleAction::Ask);
    }
}
