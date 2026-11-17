//! What a supervisor may write into the audit log's reserve over its whole life, claimed from
//! the sink when the supervisor is built.
//!
//! Only a bounded set of records may use the reserve (see [`AuditSink::reserve`]): a launch
//! record per launch (the budget's `max_launches`, a failed launch included), an exit and one
//! stop per session, two records per allow (the budget's `max_allows`: a caller's answer, and the
//! session's settlement or the record that it was not delivered), and [`MAX_REGISTRATIONS`]
//! registrations and releases. Each is at most [`record_bound`] bytes, so the product is what
//! the supervisor can ever need.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crate::audit::{AuditApproval, AuditEvent, AuditLimits, AuditRecord};
use crate::policy::{Caveat, LaunchPolicy};
use crate::spec::LaunchConfig;

/// The registrations and releases a supervisor records over its life; a further one is
/// refused ([`SupervisorError::RegistrationLimit`](crate::SupervisorError::RegistrationLimit)).
pub(crate) const MAX_REGISTRATIONS: u32 = 64;
/// Records per launch that may use the reserve: the launch, its exit and the stop that ends it.
const PER_LAUNCH: u64 = 3;
/// Records per allow that may use the reserve: a caller's answer, and either the session's
/// settlement or the record that the answer did not reach the harness (whose allowance stays
/// spent, so a retry is another allow).
const PER_ALLOW: u64 = 2;
/// The longest name a record carries: a principal, by its grammar, and a launched agent's
/// principal are both shorter.
const NAME: usize = 128;
/// The longest approval id the adapters issue (`<flavor>-<sequence>`) is far shorter.
const APPROVAL_ID: usize = 64;
/// The longest outcome code is far shorter.
const OUTCOME: &str = "outcome-code-bound-outcome-code-bound-xx";
/// Room for a timestamp wider than today's.
const SLACK: u64 = 32;

/// The bytes a supervisor enforcing `policy` over `configs` may write into the reserve over its
/// life.
pub(crate) fn demand(policy: &LaunchPolicy, configs: &[LaunchConfig]) -> u64 {
    let budget = policy.budget();
    let records = PER_LAUNCH * u64::from(budget.max_launches)
        + PER_ALLOW * u64::from(budget.max_allows)
        + u64::from(MAX_REGISTRATIONS);
    records.saturating_mul(record_bound(configs))
}

/// The longest line, newline included, any record of a supervisor over `configs` can take:
/// every field at its widest, the root the longest of the configs' once escaped.
pub(crate) fn record_bound(configs: &[LaunchConfig]) -> u64 {
    let name = "x".repeat(NAME);
    let roots = configs.iter().map(|config| Some(config.root.clone()));
    std::iter::once(None::<PathBuf>)
        .chain(roots)
        .map(|root| {
            let record = AuditRecord {
                at: SystemTime::now(),
                event: AuditEvent::ApprovalAnswer,
                caller: Some(name.clone()),
                claimed: Some(name.clone()),
                peer: Some(u32::MAX),
                caller_depth: Some(u32::MAX),
                session: Some(u64::MAX),
                agent: Some(name.clone()),
                flavor: Some(OUTCOME),
                root,
                depth: Some(u32::MAX),
                limits: Some(AuditLimits {
                    wall_time: Duration::from_secs(u64::MAX),
                    max_turns: u32::MAX,
                    max_depth: u32::MAX,
                    max_concurrent: u32::MAX,
                    max_children: u32::MAX,
                }),
                caveats: vec![Caveat::DetachedServer.id(), Caveat::UserMcpServers.id()],
                approval: Some(AuditApproval {
                    id: "x".repeat(APPROVAL_ID),
                    kind: OUTCOME,
                    allowed: false,
                    source: OUTCOME,
                }),
                outcome: OUTCOME,
                suppressed: Some(u64::MAX),
            };
            record.to_json_line().len() as u64 + 1 + SLACK
        })
        .max()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{Harness, HarnessKind};
    use crate::identity::AgentIdentity;

    #[test]
    fn the_bound_covers_a_record_with_every_field_and_grows_with_the_root() {
        let short = LaunchConfig::new(
            Harness::new(HarnessKind::Codex),
            "/r",
            AgentIdentity::new("agent").unwrap(),
        );
        let long = LaunchConfig::new(
            Harness::new(HarnessKind::Codex),
            format!("/{}\"quoted\"", "d".repeat(300)),
            AgentIdentity::new("agent").unwrap(),
        );
        let bound = record_bound(std::slice::from_ref(&short));
        let real = AuditRecord {
            caller: Some(format!("launched-opencode-{}-ffffffff", u64::MAX)),
            session: Some(u64::MAX),
            root: Some(short.root.clone()),
            caveats: vec![Caveat::DetachedServer.id(), Caveat::UserMcpServers.id()],
            ..AuditRecord::new(AuditEvent::Launch, "accepted")
        };
        assert!((real.to_json_line().len() as u64) < bound);
        assert!(record_bound(&[short, long]) > bound + 300);
    }
}
