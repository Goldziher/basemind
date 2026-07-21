#![cfg(all(feature = "replay", unix))]
//! Real-terminal (PTY) end-to-end coverage of the failure-result path (`shell:exec` exiting non-zero
//! renders the red `✗` marker and the run still stops cleanly) and of terminal resize (SIGWINCH mid-
//! session must not corrupt the transcript or hang the UI). Complements `pty_permission.rs` (the
//! allow/deny/cancel matrix) and `pty_render.rs` (success-path rendering + the `✓` color spot-check).

mod common;

use common::PtySession;
use std::time::Duration;

/// How long a "must stay absent" window dwells before a negative assertion is trusted.
const ABSENCE_DWELL: Duration = Duration::from_millis(800);

#[test]
fn a_failing_shell_exec_renders_a_red_x_result_and_the_run_still_stops_cleanly() {
    let scenario = r#"{
        "user": "run the failing command",
        "turns": [
            {
                "text": "Running the command.",
                "tools": [
                    { "id": "c1", "name": "shell:exec", "args": { "command": "exit 3" } }
                ]
            },
            { "text": "Handled the failure." }
        ]
    }"#;
    let mut session = PtySession::spawn(scenario);

    session.expect_screen("permission required");
    session.allow_session();

    session.expect_all(&["✗", "Handled the failure.", "idle (Stop)"]);

    // The turn must keep going past a failed tool result rather than aborting on it — the closing ~keep
    // stop text is already on screen above, so confirm the overlay never comes back either. ~keep
    session.expect_absent("permission required", ABSENCE_DWELL);

    // Locate the ✗ glyph the same way pty_render.rs locates ✓: `str::find` is byte-indexed but the ~keep
    // row also holds multi-byte border glyphs, so scan by char index instead. ~keep
    let screen = session.screen();
    let (row, col) = screen
        .lines()
        .enumerate()
        .find_map(|(row, line)| {
            line.chars()
                .position(|ch| ch == '✗')
                .map(|col| (row as u16, col as u16))
        })
        .expect("failure mark present on screen");

    let cell = session.cell(row, col).expect("cell at the failure-mark position");
    assert_eq!(cell.ch, "✗", "cell contents must match the located glyph");
    assert_eq!(
        cell.fg,
        vt100::Color::Idx(1),
        "the failure mark must render in ANSI red (Color::Red -> vt100 Idx(1))"
    );
}

#[test]
fn a_success_then_a_failure_renders_both_marks() {
    // The remember-cache keys on the exact claim signature (`exec:<command>`), so "allow for ~keep
    // session" only covers repeats of the identical command — these two calls use different ~keep
    // commands, so each one raises and must answer its own overlay. ~keep
    let scenario = r#"{
        "user": "run one that works, then one that fails",
        "turns": [
            {
                "text": "Running the first command.",
                "tools": [
                    { "id": "c1", "name": "shell:exec", "args": { "command": "echo OKMARK-1" } }
                ]
            },
            {
                "text": "Running the second command.",
                "tools": [
                    { "id": "c2", "name": "shell:exec", "args": { "command": "exit 4" } }
                ]
            },
            { "text": "Both handled." }
        ]
    }"#;
    let mut session = PtySession::spawn(scenario);

    session.expect_screen("permission required");
    session.allow_session();
    session.expect_all(&["✓", "OKMARK-1"]);

    session.expect_screen("permission required");
    session.allow_session();
    session.expect_all(&["✗", "idle (Stop)"]);
}

#[test]
fn resize_mid_session_preserves_content_and_the_ui_keeps_rendering() {
    let scenario = r#"{
        "user": "run the resize marker command",
        "turns": [
            {
                "text": "Running the command.",
                "tools": [
                    { "id": "c1", "name": "shell:exec", "args": { "command": "echo RESIZE-OK" } }
                ]
            },
            { "text": "Done." }
        ]
    }"#;
    let mut session = PtySession::spawn(scenario);

    session.expect_screen("permission required");
    session.allow_session();
    session.expect_all(&["RESIZE-OK", "idle (Stop)"]);

    // Grow the terminal; the SIGWINCH-driven repaint is async, so poll rather than assert immediately. ~keep
    session.resize(40, 120);
    session.expect_all(&[" transcript ", "RESIZE-OK", " mock/scripted "]);

    // Shrink back to the original size; the same invariants must still hold after the second resize. ~keep
    session.resize(24, 80);
    session.expect_all(&[" transcript ", "RESIZE-OK", " mock/scripted "]);
}
