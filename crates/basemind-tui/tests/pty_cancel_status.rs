#![cfg(all(feature = "replay", unix))]
//! Real-terminal (PTY) end-to-end coverage of cancel/interrupt behavior and the status bar: proves
//! Esc cancels a suspended (permission-gated) turn AND a mid-flight running tool, and that the status
//! bar's model chip, token counter, and in-flight/idle input title all track a real turn over the
//! actual crossterm/raw-mode UI.

mod common;

use common::PtySession;
use std::time::Duration;

/// How long a "must stay absent" window dwells before a negative assertion is trusted.
const ABSENCE_DWELL: Duration = Duration::from_millis(800);

#[test]
fn esc_at_the_permission_prompt_cancels_the_turn_without_running_the_command() {
    let scenario = r#"{
        "user": "cancel at the gate",
        "turns": [
            {
                "text": "About to run the gated command.",
                "tools": [
                    { "id": "c1", "name": "shell:exec", "args": { "command": "echo SHOULD-NOT-RUN" } }
                ]
            }
        ]
    }"#;
    let mut session = PtySession::spawn(scenario);

    session.expect_screen("permission required");
    session.esc();

    session.expect_screen("idle (Cancelled)");
    session.expect_absent("SHOULD-NOT-RUN", ABSENCE_DWELL);
}

#[test]
fn esc_cancels_a_long_running_exec_while_it_is_still_in_flight() {
    let scenario = r#"{
        "user": "cancel mid-run",
        "turns": [
            {
                "text": "Running a slow command.",
                "tools": [
                    { "id": "c1", "name": "shell:exec", "args": { "command": "sleep 5" } }
                ]
            }
        ]
    }"#;
    let mut session = PtySession::spawn(scenario);

    // The exec only starts once the gate is approved, so approve first, then catch the run ~keep
    // in-flight before cancelling it well short of the full 5s sleep. ~keep
    session.expect_screen("permission required");
    session.allow_session();

    session.expect_screen("thinking…");
    session.esc();

    session.expect_screen("idle (Cancelled)");
}

#[test]
fn status_bar_shows_model_chip_token_counter_and_in_flight_then_idle_title() {
    let scenario = r#"{
        "user": "show status",
        "turns": [
            {
                "text": "Running a quick command.",
                "tools": [
                    { "id": "c1", "name": "shell:exec", "args": { "command": "echo STATUS-OK" } }
                ]
            },
            { "text": "Done." }
        ]
    }"#;
    let mut session = PtySession::spawn(scenario);

    session.expect_screen("permission required");
    // The model chip is present from the very first frame and stays put through the whole run. ~keep
    assert!(
        session.screen().contains("mock/scripted"),
        "model chip present before approval\n{}",
        session.screen()
    );

    // While the gated turn is suspended, the input title already reads as in-flight (a turn is ~keep
    // running even though it is parked on the permission overlay). ~keep
    session.expect_screen("turn in progress");

    session.allow_session();
    session.expect_all(&["STATUS-OK", "idle (Stop)"]);

    // Back at idle, the title reverts to the idle variant and the model chip is still present. ~keep
    session.expect_screen("Enter send · Esc quit · Ctrl-C exit");
    let screen = session.screen();
    assert!(screen.contains("mock/scripted"), "model chip present at idle\n{screen}");
    assert!(screen.contains("in "), "token counter 'in ' present\n{screen}");
    assert!(screen.contains("/ out"), "token counter '/ out' present\n{screen}");
    assert!(screen.contains("tok"), "token counter 'tok' unit present\n{screen}");
}

#[test]
fn ctrl_c_exits_cleanly_without_hanging() {
    let scenario = r#"{
        "user": "just exit",
        "turns": [
            { "text": "Nothing to do here." }
        ]
    }"#;
    let mut session = PtySession::spawn(scenario);

    session.expect_screen("idle (Stop)");
    session.ctrl_c();

    // The main assertion is that this test returns at all: an explicit Ctrl-C must not hang the ~keep
    // process. Drop() would also send Ctrl-C and reap the child, so this simply proves the same ~keep
    // path works when driven explicitly rather than only on teardown. ~keep
    session.expect_absent("permission required", ABSENCE_DWELL);
}
