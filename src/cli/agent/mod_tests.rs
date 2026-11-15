//! Who a command acts as, and how a refusal becomes an exit code.

use super::test_support::{JSON, TEXT, caller, capture};
use super::*;
use crate::cli::exit::{ERROR, USAGE, exit_code_for};
use crate::comms::v2::{Actor, IdKind, InvalidId, ReplayKey, RepoKey};

/// Longest principal the id grammar allows: any sub-identity of it is over the id length limit.
const LONGEST_PRINCIPAL_LEN: usize = 128;

#[test]
fn acting_as_nobody_is_the_caller() {
    let base = caller("a");

    assert_eq!(acting_as(&base, None), Ok(base.clone()));
    assert_eq!(acting_as(&base, Some("")), Ok(base));
}

#[test]
fn acting_as_a_name_keeps_the_repository_and_the_replay_key() {
    let base = caller("a").with_replay_key(ReplayKey::parse("relay-1").expect("replay key"));

    let acting = acting_as(&base, Some("helper")).expect("composed");

    assert_eq!(acting.actor.as_str(), "a.helper");
    assert_eq!(acting.actor.principal(), "a");
    assert_eq!(acting.repo, base.repo);
    assert_eq!(acting.replay_key, base.replay_key);
}

#[test]
fn acting_as_a_name_from_a_sub_identity_stays_under_the_principal() {
    let base = caller("a.helper");

    let acting = acting_as(&base, Some("other")).expect("composed");

    assert_eq!(acting.actor.as_str(), "a.other");
}

#[test]
fn acting_as_a_malformed_name_is_refused() {
    assert_eq!(
        acting_as(&caller("a"), Some("Bad Name")),
        Err(CommsError::InvalidId(InvalidId::new(IdKind::Sub, "Bad Name")))
    );
}

#[test]
fn acting_as_a_name_that_makes_the_id_too_long_is_refused() {
    let principal = "p".repeat(LONGEST_PRINCIPAL_LEN);
    let base = Caller::new(
        Actor::parse(principal).expect("a principal at the length limit"),
        RepoKey::parse("github.com/acme/app").expect("repository key"),
    );

    assert!(matches!(
        acting_as(&base, Some("helper")),
        Err(CommsError::InvalidId(_))
    ));
}

#[test]
fn refused_is_a_usage_error_and_failed_a_runtime_error() {
    let error = CommsError::EmptyBody;

    assert_eq!(exit_code_for(&refused("agent post", &error)), USAGE);
    assert_eq!(exit_code_for(&failed("agent post", &error)), ERROR);
    assert_eq!(
        refused("agent post", &error).to_string(),
        "agent post: `body` is empty: write the message to post"
    );
    assert_eq!(
        failed("agent post", &error).to_string(),
        "agent post: `body` is empty: write the message to post"
    );
}

#[test]
fn run_report_renders_what_the_operation_returned() {
    let report: Result<serde_json::Value, CommsError> = Ok(serde_json::json!({"threads": 2}));

    let text = capture(|out| run_report("agent status", report.clone(), &TEXT, out));
    let json = capture(|out| run_report("agent status", report, &JSON, out));

    assert_eq!(text.expect("rendered"), "threads: 2\n");
    assert_eq!(json.expect("rendered"), "{\n  \"threads\": 2\n}\n");
}

#[test]
fn run_report_of_a_failure_is_a_runtime_error() {
    let report: Result<serde_json::Value, CommsError> = Err(CommsError::Backend {
        detail: "the daemon is not running".to_string(),
    });

    let error = capture(|out| run_report("agent cleanup", report, &TEXT, out)).expect_err("fails");

    assert_eq!(exit_code_for(&error), ERROR);
    assert!(
        error
            .to_string()
            .starts_with("agent cleanup: the comms store failed: the daemon is not running"),
        "{error}"
    );
}
