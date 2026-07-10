//! End-to-end smoke test for the `basemind comms` CLI against a REAL detached broker daemon.
//!
//! This exercises the actual `comms daemon` process path — the one the unit suites miss because
//! they drive an in-process `Broker`/`InProcFrontend` with a test runtime. It is the regression
//! guard for the "bind the socket inside the tokio runtime" fix: if the daemon ever again binds
//! its socket outside a reactor, `comms start` panics + times out and this test fails.
//!
//! It also pins the **condensation** contract end to end: `comms history --json` for a different
//! agent returns the message front-matter (subject) but NEVER the body bytes.
//!
//! Runs on Unix and Windows. `comms start` previously hung forever on Windows: the detached daemon
//! inherited the launcher's stdout/stderr (Windows `CreateProcess` with `bInheritHandles = TRUE`
//! leaks every inheritable handle, including the pipe `Command::output()` captures), so the capturing
//! parent never saw EOF and blocked until the daemon died. `spawn_detached_daemon` now clears the
//! inherit bit on its std handles before the detached spawn, so the daemon leaks none of them. The
//! in-process + direct-daemon-spawn suites in `comms_smoke.rs` cover the broker itself.

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

    let (ok, _out, err) = comms(&comms_dir, "agent-alice", &["start"]);
    assert!(ok, "comms start failed: {err}");

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

    let (ok, history, e) = comms(
        &comms_dir,
        "agent-bob",
        &["history", "--root", &root, "devroom", "--json"],
    );
    assert!(ok, "history failed: {e}");

    assert!(
        history.contains("Hello team"),
        "history should carry the subject front-matter, got: {history}"
    );
    assert!(
        history.contains("agent-alice"),
        "history should carry the sender, got: {history}"
    );
    assert!(
        !history.contains(BODY),
        "history lookup must be front-matter only — the body leaked: {history}"
    );

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

/// `room-for-path` resolves a path to its canonical repo room and joins it: for a non-repo temp
/// directory the scope is `path` and the room id is non-empty (derived from the path). A second
/// agent resolving the SAME path lands on the identical room id — the get-or-create is idempotent.
#[test]
fn room_for_path_resolves_and_joins_a_path_scoped_room() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let root = tmp.path().to_string_lossy().into_owned();

    let (ok, _o, err) = comms(&comms_dir, "agent-alice", &["start"]);
    assert!(ok, "comms start failed: {err}");

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

    let (ok, out, e) = comms(
        &comms_dir,
        "agent-alice",
        &["room-for-path", "--root", &root, &root, "--json"],
    );
    assert!(ok, "room-for-path failed: {e}");
    assert!(
        out.contains("\"scope\":\"path\""),
        "a non-repo path resolves to a path-scoped room, got: {out}"
    );
    let room = out
        .split("\"room\":\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("room id in room-for-path json");
    assert!(!room.is_empty(), "room id must be non-empty, got: {out}");

    let (ok, out2, e) = comms(
        &comms_dir,
        "agent-bob",
        &["room-for-path", "--root", &root, &root, "--json"],
    );
    assert!(ok, "second room-for-path failed: {e}");
    assert!(
        out2.contains(&format!("\"room\":\"{room}\"")),
        "the same path must resolve to the same room id, got: {out2}"
    );
}

/// The `dm` verb plus `--as-agent` deliver a direct message to one agent's inbox via the private
/// pairwise `dm:<lo>:<hi>` room: the sender (selected with `--as-agent`) creates + joins + posts,
/// the recipient is auto-joined by the verb, and `inbox --as-agent <recipient>` surfaces it — while
/// the sender's own inbox stays empty (server-side self-exclusion). The default-identity process
/// here carries neither identity; both are chosen purely via `--as-agent`.
#[test]
fn dm_verb_delivers_to_recipient_inbox_via_pairwise_room() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let root = tmp.path().to_string_lossy().into_owned();

    let (ok, _o, err) = comms(&comms_dir, "agent-default", &["start"]);
    assert!(ok, "comms start failed: {err}");

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

    let (ok, send_out, e) = comms(
        &comms_dir,
        "agent-default",
        &[
            "dm",
            "--root",
            &root,
            "--as-agent",
            "alice",
            "--to",
            "bob",
            "--subject",
            "ping",
            "--body",
            "pong",
            "--json",
        ],
    );
    assert!(ok, "dm send failed: {e}");
    assert!(
        send_out.contains("\"room\":\"dm:alice:bob\""),
        "dm output should name the pairwise room, got: {send_out}"
    );
    assert!(
        send_out.contains("\"message_id\""),
        "dm output should carry the message_id, got: {send_out}"
    );

    let (ok, inbox, e) = comms(
        &comms_dir,
        "agent-default",
        &["inbox", "--root", &root, "--as-agent", "bob", "--json"],
    );
    assert!(ok, "bob inbox failed: {e}");
    assert!(
        inbox.contains("\"subject\":\"ping\"") && inbox.contains("\"from\":\"alice\""),
        "bob's inbox should carry the DM front-matter, got: {inbox}"
    );

    let (ok, alice_inbox, e) = comms(
        &comms_dir,
        "agent-default",
        &["inbox", "--root", &root, "--as-agent", "alice", "--json"],
    );
    assert!(ok, "alice inbox failed: {e}");
    assert!(
        alice_inbox.contains("\"total\":0"),
        "the sender must not see its own DM in its inbox, got: {alice_inbox}"
    );

    let (ok, _o, self_err) = comms(
        &comms_dir,
        "agent-default",
        &[
            "dm",
            "--root",
            &root,
            "--as-agent",
            "carol",
            "--to",
            "carol",
            "--subject",
            "x",
        ],
    );
    assert!(!ok, "dm to self should fail");
    assert!(
        self_err.contains("cannot dm yourself"),
        "self-dm error should be explicit, got: {self_err}"
    );
}
