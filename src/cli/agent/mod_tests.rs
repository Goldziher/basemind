//! Who a command acts as, and how a refusal becomes an exit code.

use super::test_support::{JSON, TEXT, capture, owner};
use super::*;
use crate::cli::exit::{ERROR, USAGE, exit_code_for};
use crate::comms::v2::ReplayKey;

/// Longest principal the id grammar allows: any sub-identity of it is over the id length limit.
const LONGEST_PRINCIPAL_LEN: usize = 128;

#[test]
fn a_command_without_as_acts_as_the_sessions_principal() {
    let owner = owner("a");

    for as_name in [None, Some("")] {
        let acting = caller(&owner.session(), "agent post", as_name).expect("the principal");

        assert_eq!(acting.actor.as_str(), "a");
        assert_eq!(acting.repo.as_str(), "github.com/acme/app");
        assert_eq!(acting.replay_key, None);
    }
}

#[test]
fn as_names_a_sub_identity_and_keeps_the_repository_and_the_replay_key() {
    let owner = owner("a");
    let key = ReplayKey::parse("relay-1").expect("replay key");
    let session = Session {
        replay_key: Some(&key),
        ..owner.session()
    };

    let acting = caller(&session, "agent post", Some("helper")).expect("composed");

    assert_eq!(acting.actor.as_str(), "a.helper");
    assert_eq!(acting.actor.principal(), "a");
    assert_eq!(acting.repo.as_str(), "github.com/acme/app");
    assert_eq!(acting.replay_key, Some(key));
}

#[test]
fn a_malformed_as_is_a_usage_error_that_names_the_flag() {
    let error = caller(&owner("a").session(), "agent inbox", Some("Bad Name")).expect_err("refused");

    assert_eq!(exit_code_for(&error), USAGE);
    assert_eq!(
        error.to_string(),
        "agent inbox: `as`: invalid sub-identity name \"Bad Name\": expected 1 to 32 characters of a-z, 0-9, `_` \
         or `-`"
    );
}

#[test]
fn an_as_that_makes_the_id_too_long_is_refused() {
    let longest = owner(&"p".repeat(LONGEST_PRINCIPAL_LEN));

    assert!(caller(&longest.session(), "agent post", None).is_ok());
    let error = caller(&longest.session(), "agent post", Some("helper")).expect_err("refused");

    assert_eq!(exit_code_for(&error), USAGE);
    assert!(
        error.to_string().starts_with("agent post: `as`: invalid agent id "),
        "{error}"
    );
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
