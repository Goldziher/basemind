//! End-to-end smoke test for the `basemind comms` CLI against a REAL detached broker daemon.
//!
//! This exercises the actual `comms daemon` process path — the one the unit suites miss because
//! they drive an in-process `Broker`/`InProcFrontend` with a test runtime. It is the regression
//! guard for the "bind the socket inside the tokio runtime" fix: if the daemon ever again binds
//! its socket outside a reactor, `comms start` panics + times out and this test fails.
//!
//! It also pins the **condensation** contract end to end: `comms history --json` for a different
//! agent returns the message front-matter (subject) but NEVER the body bytes.

#![cfg(feature = "comms")]

use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_basemind");

/// Run `basemind comms <args...>` as `agent` against the isolated `comms_dir`, returning
/// `(success, stdout, stderr)`.
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

#[test]
fn comms_daemon_round_trip_history_is_front_matter_only() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let root = tmp.path().to_string_lossy().into_owned();
    let scope = format!("path:{root}");
    const BODY: &str = "SECRET-BODY-must-never-appear-in-history-lookups";

    // 1. Start the detached daemon. This is the path that panicked before the runtime fix.
    let (ok, _out, err) = comms(&comms_dir, "agent-alice", &["start"]);
    assert!(ok, "comms start failed: {err}");

    // Always tear the daemon down, even if an assertion below panics.
    struct Stop<'a>(&'a Path);
    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            let _ = Command::new(BIN)
                .args(["comms", "stop"])
                .env("BASEMIND_COMMS_DIR", self.0)
                .output();
        }
    }
    let _stop = Stop(&comms_dir);

    // 2. Alice creates a path-scoped room and posts a message with a long body.
    let (ok, _o, e) = comms(
        &comms_dir,
        "agent-alice",
        &["room-create", "--root", &root, "--scope", &scope, "devroom"],
    );
    assert!(ok, "room-create failed: {e}");
    let (ok, _o, e) = comms(
        &comms_dir,
        "agent-alice",
        &["post", "--root", &root, "--body", BODY, "devroom", "Hello team"],
    );
    assert!(ok, "post failed: {e}");

    // 3. A DIFFERENT agent reads the history as JSON. Bob auto-joins the path-scoped room
    //    because his cwd-derived scope (the same `root`) covers it — no explicit join.
    let (ok, history, e) = comms(
        &comms_dir,
        "agent-bob",
        &["history", "--root", &root, "devroom", "--json"],
    );
    assert!(ok, "history failed: {e}");

    // The front-matter (subject + sender) is present...
    assert!(
        history.contains("Hello team"),
        "history should carry the subject front-matter, got: {history}"
    );
    assert!(
        history.contains("agent-alice"),
        "history should carry the sender, got: {history}"
    );
    // ...but the body bytes are NEVER loaded into a lookup.
    assert!(
        !history.contains(BODY),
        "history lookup must be front-matter only — the body leaked: {history}"
    );

    // 4. The body is reachable only via the explicit body path.
    // Pull the message id out of the history JSON (…"id":"devroom:agent-alice:<ts>:<seq>"…).
    let id = history
        .split("\"id\":\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("message id in history json")
        .to_string();
    let (ok, body, e) = comms(&comms_dir, "agent-bob", &["read", "--root", &root, &id]);
    assert!(ok, "read body failed: {e}");
    assert!(
        body.contains(BODY),
        "the explicit body path must return the body, got: {body}"
    );
}
