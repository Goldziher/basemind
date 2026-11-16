//! End-to-end smoke test of the `basemind agent` CLI against a REAL detached daemon: one agent
//! starts an invitation-only thread, the other reads it (reading marks read), the operator's
//! `cleanup --archive` closes a thread its caller did not create, and `status` reports the store.
//!
//! Runs on Unix and Windows.

#![cfg(feature = "comms")]

use std::path::Path;
use std::process::Command;

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_basemind");

/// Run `basemind --json --root <root> agent <args...>` as `agent` against the isolated `home`.
fn agent(home: &Path, root: &Path, agent: &str, args: &[&str]) -> (bool, Value, String) {
    let out = Command::new(BIN)
        .arg("--json")
        .arg("--root")
        .arg(root)
        .arg("agent")
        .args(args)
        .env("BASEMIND_COMMS_DIR", home.join("comms"))
        .env("BASEMIND_DATA_HOME", home)
        .env("BASEMIND_AGENT_ID", agent)
        .output()
        .expect("spawn basemind");
    let stdout = String::from_utf8_lossy(&out.stdout);
    (
        out.status.success(),
        serde_json::from_str(stdout.trim()).unwrap_or(Value::Null),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Guard that stops the daemon on drop.
struct Stop<'a>(&'a Path);
impl Drop for Stop<'_> {
    fn drop(&mut self) {
        let _ = Command::new(BIN)
            .args(["comms", "stop"])
            .env("BASEMIND_COMMS_DIR", self.0.join("comms"))
            .env("BASEMIND_DATA_HOME", self.0)
            .output();
    }
}

#[test]
fn agents_talk_and_the_operator_archives_through_the_cli() {
    let home = tempfile::tempdir().expect("tempdir");
    let repo = tempfile::tempdir().expect("repo");
    let (home, root) = (home.path(), repo.path());
    let _stop = Stop(home);

    let (ok, posted, e) = agent(
        home,
        root,
        "agent-alice",
        &[
            "post",
            "--subject",
            "hand-off",
            "--to",
            "agent-bob",
            "--body",
            "take src/auth",
        ],
    );
    assert!(ok, "post failed: {e}");
    let thread = posted["thread"].as_str().expect("a thread id").to_string();
    assert_eq!(posted["created"], true, "{posted}");

    let (ok, read, e) = agent(home, root, "agent-bob", &["inbox"]);
    assert!(ok, "inbox failed: {e}");
    assert_eq!(read["messages"][0]["body"], "take src/auth", "{read}");
    assert_eq!(read["messages"][0]["from"], "agent-alice", "{read}");
    let (ok, again, e) = agent(home, root, "agent-bob", &["inbox"]);
    assert!(ok, "inbox failed: {e}");
    assert_eq!(
        again["messages"],
        Value::Array(Vec::new()),
        "reading marked read: {again}"
    );

    let (ok, refused, e) = agent(home, root, "agent-bob", &["threads", "--thread", &thread, "--archive"]);
    assert!(!ok, "only the creator archives through the tool's command: {refused}");
    assert!(e.contains("creator"), "{e}");
    let (ok, cleaned, e) = agent(home, root, "agent-bob", &["cleanup", "--archive", &thread]);
    assert!(ok, "cleanup failed: {e}");
    assert_eq!(cleaned["archived"][0], thread.as_str(), "{cleaned}");

    let (ok, _, e) = agent(
        home,
        root,
        "agent-alice",
        &["post", "--thread", &thread, "--body", "late"],
    );
    assert!(!ok, "an archived thread takes no posts");
    assert!(e.contains("archived"), "{e}");

    let (ok, status, e) = agent(home, root, "agent-alice", &["status"]);
    assert!(ok, "status failed: {e}");
    assert_eq!(status["principal"], "agent-alice", "{status}");
    assert_eq!(status["store"]["state"], "open", "{status}");
}
