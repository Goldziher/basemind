#![cfg(all(feature = "replay", unix))]
//! Real-terminal (PTY) end-to-end coverage of input editing, scrolling, and multi-turn submission:
//! typed characters and backspace against the live block-cursor glyph, the Enter no-op-on-empty vs
//! submit-and-clear contract, a second PTY-typed message driving a second scripted turn, and the
//! transcript's Page Up / Page Down scroll offset.

mod common;

use common::PtySession;
use std::time::Duration;

/// How long a "must stay absent" window dwells before a negative assertion is trusted.
const ABSENCE_DWELL: Duration = Duration::from_millis(500);

/// A single-stop scenario: the auto-sent user message gets one tool-free reply, so the run goes
/// idle with nothing left scripted. `main.rs` mirrors the auto-sent prompt into the transcript, so
/// it renders a `you: start` line before any of these tests type their own message.
fn single_turn_scenario() -> String {
    r#"{
        "user": "start",
        "turns": [
            { "text": "Ready X1." }
        ]
    }"#
    .to_string()
}

/// A two-stop-turn scenario: turn 1 answers the auto-sent user message, turn 2 answers a SECOND
/// message typed over the PTY. Both turns are tool-free, so each stops the run outright.
fn two_turn_scenario() -> String {
    r#"{
        "user": "hello one",
        "turns": [
            { "text": "Reply ONE-A9." },
            { "text": "Reply TWO-B7." }
        ]
    }"#
    .to_string()
}

/// A single tool-free turn whose reply is taller than the 24-row terminal, so the transcript
/// viewport must scroll to reach both ends. Lines are numbered `line-01`..`line-40` plus a
/// distinctive tail marker, so both edges of the scroll range are unambiguous needles.
fn tall_transcript_scenario() -> String {
    let mut body = (1..=40).map(|n| format!("line-{n:02}")).collect::<Vec<_>>().join("\n");
    body.push_str("\nBOTTOM-MARKER-Q");
    format!(r#"{{ "user": "start", "turns": [ {{ "text": {body:?} }} ] }}"#)
}

#[test]
fn typing_renders_characters_immediately_before_the_block_cursor() {
    let mut session = PtySession::spawn(&single_turn_scenario());
    session.expect_screen("idle (Stop)");

    session.type_str("abcde");
    session.expect_screen("abcde█");
}

#[test]
fn backspace_deletes_the_trailing_character_before_the_cursor() {
    let mut session = PtySession::spawn(&single_turn_scenario());
    session.expect_screen("idle (Stop)");

    session.type_str("abcde");
    session.expect_screen("abcde█");

    session.backspace();
    session.backspace();
    session.expect_screen("abc█");
    session.expect_absent("abcde█", ABSENCE_DWELL);
}

#[test]
fn enter_on_empty_input_is_a_no_op() {
    let mut session = PtySession::spawn(&single_turn_scenario());
    session.expect_screen("idle (Stop)");
    let you_count_before = session.screen().matches("you:").count();

    session.enter();

    // An incorrectly-submitted empty message would start a new turn and flip the status to ~keep
    // "thinking…"; poll for it staying absent so a repaint race cannot mask a regression. ~keep
    session.expect_absent("thinking", ABSENCE_DWELL);
    assert_eq!(
        session.screen().matches("you:").count(),
        you_count_before,
        "an empty Enter must not push a new transcript entry"
    );
}

#[test]
fn enter_on_a_typed_prompt_submits_it_and_clears_the_input_box() {
    // Two scripted stop-turns: one for the auto-sent prompt, one for the message typed below. The
    // second reply is the unambiguous signal that Enter actually submitted — the auto-sent prompt
    // already renders a `you:` line, so `you:` alone no longer proves the typed prompt went through. ~keep
    let mut session = PtySession::spawn(&two_turn_scenario());
    session.expect_all(&["Reply ONE-A9.", "idle (Stop)"]);

    session.type_str("PROMPT-Z3");
    session.expect_screen("PROMPT-Z3█");

    session.enter();

    // The agent's second reply only renders once the typed prompt was submitted; only then is it safe
    // to assert the input box cleared (the block cursor no longer trails the text). ~keep
    session.expect_screen("Reply TWO-B7.");
    session.expect_absent("PROMPT-Z3█", ABSENCE_DWELL);
    session.expect_screen("PROMPT-Z3");
}

#[test]
fn a_second_pty_typed_prompt_drives_the_scripted_second_turn() {
    let mut session = PtySession::spawn(&two_turn_scenario());
    session.expect_all(&["Reply ONE-A9.", "idle (Stop)"]);

    session.type_str("second question");
    session.enter();

    session.expect_all(&["you:", "second question", "Reply TWO-B7.", "idle (Stop)"]);
}

#[test]
fn the_transcript_auto_follows_the_newest_and_page_up_down_detaches_then_re_follows() {
    let mut session = PtySession::spawn(&tall_transcript_scenario());
    session.expect_screen("idle (Stop)");

    // The transcript auto-follows the newest content, so the tail marker is visible immediately and ~keep
    // the first numbered line is scrolled off above the fold. ~keep
    session.expect_screen("BOTTOM-MARKER-Q");
    session.expect_absent("line-01", ABSENCE_DWELL);

    // Paging up detaches from the bottom (`PAGE_SCROLL` = 10 lines each) and brings the head of the ~keep
    // transcript into view; four pages saturate at the top. ~keep
    session.page_up();
    session.page_up();
    session.page_up();
    session.page_up();
    session.expect_screen("line-01");
    session.expect_absent("BOTTOM-MARKER-Q", ABSENCE_DWELL);

    // Paging back down past the bottom re-engages follow, so the tail marker returns and the head ~keep
    // scrolls out again. ~keep
    session.page_down();
    session.page_down();
    session.page_down();
    session.page_down();
    session.expect_screen("BOTTOM-MARKER-Q");
    session.expect_absent("line-01", ABSENCE_DWELL);
}
