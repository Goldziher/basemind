//! End-to-end smoke test for the `basemind comms` CLI (thread model) against a REAL detached
//! broker daemon.
//!
//! This exercises the actual `comms daemon` process path. It is the regression guard for the
//! "bind the socket inside the tokio runtime" fix and pins the **condensation** contract end to
//! end: `comms history --json` for a different agent returns the message front-matter (subject)
//! but NEVER the body bytes. It also covers the human-admin `archive` verb.
//!
//! Runs on Unix and Windows.

#![cfg(feature = "comms")]

use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_basemind");

/// Run `basemind comms <args...>` as `agent` against the isolated `comms_dir`.
fn comms(comms_dir: &Path, agent: &str, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(BIN)
        .arg("comms")
        .args(args)
        .env("BASEMIND_COMMS_DIR", comms_dir)
        .env("BASEMIND_AGENT_ID", agent)
        .output()
        .expect("spawn basemind comms");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Guard that stops the daemon on drop.
struct Stop<'a>(&'a Path);
impl Drop for Stop<'_> {
    fn drop(&mut self) {
        let _ = Command::new(BIN)
            .args(["comms", "stop"])
            .env("BASEMIND_COMMS_DIR", self.0)
            .output();
    }
}

/// Extract a JSON string field's value, e.g. `"id":"th-..."` → `th-...`.
fn json_str_field<'a>(haystack: &'a str, field: &str) -> Option<&'a str> {
    let needle = format!("\"{field}\":\"");
    haystack.split(&needle).nth(1).and_then(|s| s.split('"').next())
}

#[test]
fn comms_daemon_thread_history_is_front_matter_only() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let root = tmp.path().to_string_lossy().into_owned();
    const BODY: &str = "SECRET-BODY-must-never-appear-in-history-lookups";

    let (ok, _out, err) = comms(&comms_dir, "agent-alice", &["start"]);
    assert!(ok, "comms start failed: {err}");
    let _stop = Stop(&comms_dir);

    // Start a thread addressed by subject + a member (two dimensions).
    let (ok, start_out, e) = comms(
        &comms_dir,
        "agent-alice",
        &[
            "thread-start",
            "--root",
            &root,
            "--subject",
            "Hello team",
            "--member",
            "agent-bob",
            "--json",
        ],
    );
    assert!(ok, "thread-start failed: {e}");
    let thread = json_str_field(&start_out, "id")
        .expect("thread id in start json")
        .to_string();

    let (ok, _o, e) = comms(
        &comms_dir,
        "agent-alice",
        &["post", "--root", &root, "--body", BODY, &thread, "Hello team"],
    );
    assert!(ok, "post failed: {e}");

    let (ok, history, e) = comms(
        &comms_dir,
        "agent-bob",
        &["history", "--root", &root, &thread, "--json"],
    );
    assert!(ok, "history failed: {e}");
    assert!(
        history.contains("Hello team"),
        "history carries the subject, got: {history}"
    );
    assert!(
        history.contains("agent-alice"),
        "history carries the sender, got: {history}"
    );
    assert!(
        !history.contains(BODY),
        "history lookup must be front-matter only — the body leaked: {history}"
    );

    let id = json_str_field(&history, "id")
        .expect("message id in history json")
        .to_string();
    let (ok, body, e) = comms(&comms_dir, "agent-bob", &["read", "--root", &root, &id]);
    assert!(ok, "read body failed: {e}");
    assert!(
        body.contains(BODY),
        "the explicit body path must return the body, got: {body}"
    );
}

/// The human-admin `archive` verb: the creator archives a thread, and it then only shows under
/// `threads --include-archived`.
#[test]
fn thread_archive_removes_from_active_listing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let root = tmp.path().to_string_lossy().into_owned();

    let (ok, _o, err) = comms(&comms_dir, "agent-alice", &["start"]);
    assert!(ok, "comms start failed: {err}");
    let _stop = Stop(&comms_dir);

    let (ok, start_out, e) = comms(
        &comms_dir,
        "agent-alice",
        &[
            "thread-start",
            "--root",
            &root,
            "--subject",
            "planning",
            "--member",
            "agent-bob",
            "--json",
        ],
    );
    assert!(ok, "thread-start failed: {e}");
    let thread = json_str_field(&start_out, "id").expect("thread id").to_string();

    let (ok, _o, e) = comms(&comms_dir, "agent-alice", &["archive", "--root", &root, &thread]);
    assert!(ok, "archive failed: {e}");

    let (ok, active, e) = comms(&comms_dir, "agent-alice", &["threads", "--root", &root, "--json"]);
    assert!(ok, "threads failed: {e}");
    assert!(
        active.contains("\"total\":0"),
        "archived thread must not be in the active listing: {active}"
    );

    let (ok, all, e) = comms(
        &comms_dir,
        "agent-alice",
        &["threads", "--root", &root, "--include-archived", "--json"],
    );
    assert!(ok, "threads --include-archived failed: {e}");
    assert!(
        all.contains(&thread),
        "include-archived must surface the archived thread: {all}"
    );
}
