#![cfg(all(feature = "replay", unix))]
//! Real-terminal (PTY) end-to-end canary for the `basemind-tui` binary — the "Playwright for TTY"
//! layer that the in-memory `TestBackend` e2e tests cannot reach.
//!
//! `TestBackend` renders the pure `App`/`ui` half in memory, but it never exercises `run.rs`, which
//! enters crossterm raw mode + the alternate screen and needs a real terminal. This test closes that
//! gap through the shared [`common::PtySession`] harness: it spawns the actual binary under a
//! pseudo-terminal, parses its live ANSI output with a `vt100` emulator, injects keystrokes, and
//! asserts on the rendered screen — proving the whole permission→exec→stop loop works over a TTY.

mod common;

use common::PtySession;

/// Unique stdout marker the scripted `shell:exec` emits; distinctive enough to never collide with
/// incidental screen text.
const MARKER: &str = "PTY-MARKER-7F3A9";

/// A hermetic, shell-only scenario: turn 1 runs `echo <marker>` behind the permission gate; turn 2
/// emits a closing line with no tools, which stops the run.
fn marker_scenario() -> String {
    format!(
        r#"{{
            "user": "run the marker",
            "turns": [
                {{
                    "text": "Running the marker now.",
                    "tools": [
                        {{ "id": "c1", "name": "shell:exec", "args": {{ "command": "echo {MARKER}" }} }}
                    ]
                }},
                {{ "text": "All done." }}
            ]
        }}"#
    )
}

#[test]
fn a_pty_replay_run_approves_a_shell_exec_and_renders_its_output() {
    let mut session = PtySession::spawn(&marker_scenario());

    // The permission-gated exec raises the overlay; approve it for the session over the PTY. ~keep
    session.expect_screen("permission required");
    session.allow_session();

    // Require BOTH the exec output marker AND the run reaching idle (only possible once the exec ~keep
    // succeeded and the tool-free turn 2 stopped) — so a stray marker echoed into the args line ~keep
    // cannot satisfy the test on its own. ~keep
    session.expect_all(&[MARKER, "idle (Stop)"]);
}
