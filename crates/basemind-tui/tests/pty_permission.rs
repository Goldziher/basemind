#![cfg(all(feature = "replay", unix))]
//! Real-terminal (PTY) end-to-end coverage of the permission engine: this proves all four answers to
//! a `shell:exec` permission prompt — allow once, allow for session, deny, and Esc-cancel — drive the
//! real crossterm/raw-mode UI the same way they drive the in-memory `App` unit tests, over an actual
//! pseudo-terminal rather than a `TestBackend`.

mod common;

use common::PtySession;
use std::time::Duration;

/// How long a "must stay absent" window dwells before a negative assertion is trusted.
const ABSENCE_DWELL: Duration = Duration::from_millis(800);

#[test]
fn allow_once_runs_the_gated_command_and_then_the_turn_stops() {
    let scenario = r#"{
        "user": "run once",
        "turns": [
            {
                "text": "Running it once.",
                "tools": [
                    { "id": "c1", "name": "shell:exec", "args": { "command": "echo ALLOW-ONCE-9K2" } }
                ]
            },
            { "text": "Done." }
        ]
    }"#;
    let mut session = PtySession::spawn(scenario);

    session.expect_screen("permission required");
    session.allow_once();

    session.expect_all(&["ALLOW-ONCE-9K2", "idle (Stop)"]);
}

#[test]
fn allow_for_session_is_remembered_and_a_repeat_of_the_same_call_never_reprompts() {
    // The remember-cache keys on the exact claim signature (`exec:<command>`), so "allow for ~keep
    // session" covers repeats of the identical command, not every future shell:exec regardless of ~keep
    // its target — hence both gated calls below run the same command. ~keep
    let scenario = r#"{
        "user": "run twice",
        "turns": [
            {
                "text": "Running the first one.",
                "tools": [
                    { "id": "c1", "name": "shell:exec", "args": { "command": "echo SESSION-7A" } }
                ]
            },
            {
                "text": "Running the second one.",
                "tools": [
                    { "id": "c2", "name": "shell:exec", "args": { "command": "echo SESSION-7A" } }
                ]
            },
            { "text": "Both done." }
        ]
    }"#;
    let mut session = PtySession::spawn(scenario);

    session.expect_screen("permission required");
    session.allow_session();
    session.expect_screen("SESSION-7A");

    // If the second, identical gated call were not covered by the remembered grant, its overlay ~keep
    // would raise and then block forever (nothing here answers a second prompt), so reaching ~keep
    // "idle (Stop)" within the default timeout is itself proof the repeat never reprompted. ~keep
    session.expect_screen("idle (Stop)");
    session.expect_absent("permission required", ABSENCE_DWELL);
}

#[test]
fn deny_fails_the_call_without_running_it_and_the_turn_still_stops() {
    // A denial is fed back into the model's history before the tool call ever "starts" (no ~keep
    // `ToolStarted` event fires for it), so no ⚙/✓/✗ transcript line renders here — the observable ~keep
    // proof of a deny is the command's output never appearing while the turn still reaches its ~keep
    // scripted stop. ~keep
    let scenario = r#"{
        "user": "run denied",
        "turns": [
            {
                "text": "Attempting the gated command.",
                "tools": [
                    { "id": "c1", "name": "shell:exec", "args": { "command": "echo DENIED-5Q" } }
                ]
            },
            { "text": "Done." }
        ]
    }"#;
    let mut session = PtySession::spawn(scenario);

    session.expect_screen("permission required");
    session.deny();

    session.expect_screen("idle (Stop)");
    session.expect_absent("DENIED-5Q", ABSENCE_DWELL);
}

#[test]
fn esc_at_the_prompt_cancels_the_turn_without_running_the_command() {
    let scenario = r#"{
        "user": "cancel me",
        "turns": [
            {
                "text": "About to run the gated command.",
                "tools": [
                    { "id": "c1", "name": "shell:exec", "args": { "command": "echo NEVER-RUN" } }
                ]
            }
        ]
    }"#;
    let mut session = PtySession::spawn(scenario);

    session.expect_screen("permission required");
    session.esc();

    session.expect_screen("idle (Cancelled)");
    session.expect_absent("NEVER-RUN", ABSENCE_DWELL);
}
