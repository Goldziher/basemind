#![cfg(all(feature = "replay", unix))]
//! Real-terminal (PTY) end-to-end coverage for streaming text, markdown rendering, and tool-call/
//! result lines. Complements `pty_replay.rs` (permission → exec → stop loop) by exercising the
//! transcript's visual contract: speaker labels, accreted streaming text, the `⚙`/`✓`/`✗` tool
//! markers, and the lightweight markdown transform in `markdown.rs`.

mod common;

use common::PtySession;
use std::time::Duration;

/// Unique stdout marker the scripted `shell:exec` emits; distinctive enough to never collide with
/// incidental screen text.
const MARKER: &str = "RENDER-OK";

/// A two-turn scenario: turn 1 streams `Line one. ` and runs a marker `shell:exec` behind the
/// permission gate; turn 2 streams `Line two.` with no tools, which stops the run.
fn streaming_scenario() -> String {
    format!(
        r#"{{
            "user": "render the lines",
            "turns": [
                {{
                    "text": "Line one. ",
                    "tools": [
                        {{ "id": "c1", "name": "shell:exec", "args": {{ "command": "echo {MARKER}" }} }}
                    ]
                }},
                {{ "text": "Line two." }}
            ]
        }}"#
    )
}

/// A single tool-free stop turn whose text exercises every markdown transform under test: an ATX
/// heading, inline bold, inline code, and both bullet marker styles.
fn markdown_scenario() -> String {
    // Two hashes: the text payload contains a literal `"#` (the heading marker), which would ~keep
    // otherwise terminate a single-hash raw string early. ~keep
    r##"{
        "user": "show me markdown",
        "turns": [
            {
                "text": "# Big Heading\n\nsome **strong** and `code` words\n\n- first\n- second"
            }
        ]
    }"##
    .to_string()
}

#[test]
fn streaming_assistant_text_accretes_and_labels_render() {
    let mut session = PtySession::spawn(&streaming_scenario());

    // The permission-gated exec raises the overlay; approve it for the session over the PTY. ~keep
    session.expect_screen("permission required");
    session.allow_session();

    // The opening "user" message is mirrored into the transcript (main.rs seeds it before the loop),
    // so it renders a `you:` label; the agent label, both streamed text chunks, the tool call line,
    // its successful result, and the run reaching idle must all render too. ~keep
    session.expect_all(&[
        "you:",
        "render the lines",
        "agent:",
        "Line one.",
        "Line two.",
        "⚙ shell:exec",
        "✓",
        MARKER,
        "idle (Stop)",
    ]);
}

#[test]
fn markdown_headings_bold_code_and_bullets_render_with_raw_markers_stripped() {
    let session = PtySession::spawn(&markdown_scenario());

    session.expect_all(&["Big Heading", "strong", "code", "• first", "• second", "idle (Stop)"]);

    // The raw markdown markers must never surface — only the transformed text above. ~keep
    let settle = Duration::from_millis(600);
    session.expect_absent("# Big Heading", settle);
    session.expect_absent("**strong**", settle);
    session.expect_absent("`code`", settle);
}

#[test]
fn tool_result_check_mark_is_styled_distinctly() {
    let mut session = PtySession::spawn(&streaming_scenario());

    session.expect_screen("permission required");
    session.allow_session();
    session.expect_all(&["✓", MARKER, "idle (Stop)"]);

    // `str::find` returns a BYTE offset, but the row also holds multi-byte border glyphs (`│` is ~keep
    // 3 bytes wide but 1 terminal column), so the column must come from a char-index scan instead. ~keep
    let screen = session.screen();
    let (row, col) = screen
        .lines()
        .enumerate()
        .find_map(|(row, line)| {
            line.chars()
                .position(|ch| ch == '✓')
                .map(|col| (row as u16, col as u16))
        })
        .expect("check mark present on screen");

    let cell = session.cell(row, col).expect("cell at check-mark position");
    assert_eq!(cell.ch, "✓", "cell contents must match the located glyph");
    assert_ne!(
        cell.fg,
        vt100::Color::Default,
        "the success check mark must not use the terminal's default foreground"
    );
}
