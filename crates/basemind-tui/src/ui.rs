//! Rendering: turn an [`App`] snapshot into a frame. Pure presentation — no state mutation.
//!
//! Layout is a transcript viewport on top, a one-line status bar, and a bordered input box at the
//! bottom. A pending permission request is drawn as a centered overlay above everything.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::app::{App, PermissionPrompt, TranscriptEntry};
use crate::markdown::render_markdown;

/// Split the frame into the stacked regions: transcript, an optional one-line room bar, status bar,
/// input box. Shared by [`draw`] and [`reconcile_scroll`] so both agree on the transcript's geometry;
/// the transcript stays index 0 whether or not the room bar is present.
fn layout(area: Rect, has_roster: bool) -> std::rc::Rc<[Rect]> {
    let mut constraints = vec![Constraint::Min(3)]; // transcript ~keep
    if has_roster {
        constraints.push(Constraint::Length(1)); // room bar ~keep
    }
    constraints.push(Constraint::Length(1)); // status bar ~keep
    constraints.push(Constraint::Length(3)); // input box ~keep
    Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area)
}

/// Draw the whole UI for the current [`App`] snapshot.
pub fn draw(frame: &mut Frame, app: &App) {
    let has_roster = !app.roster.is_empty();
    let chunks = layout(frame.area(), has_roster);

    draw_transcript(frame, app, chunks[0]);
    let mut next = 1;
    if has_roster {
        draw_room_bar(frame, app, chunks[next]);
        next += 1;
    }
    draw_status(frame, app, chunks[next]);
    draw_input(frame, app, chunks[next + 1]);

    if let Some(prompt) = &app.pending_permission {
        draw_permission_overlay(frame, prompt);
    }
}

/// Build the styled, wrapped transcript lines for the current [`App`] snapshot.
fn transcript_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for entry in &app.transcript {
        match entry {
            TranscriptEntry::User(text) => push_message(&mut lines, "you", Color::Cyan, text),
            TranscriptEntry::Assistant(text) => push_message(&mut lines, "agent", Color::Green, text),
            TranscriptEntry::Tool { name, args, result, .. } => {
                lines.push(Line::from(vec![
                    Span::styled(format!("⚙ {name} "), Style::default().fg(Color::Yellow)),
                    Span::styled(args.clone(), Style::default().fg(Color::DarkGray)),
                ]));
                if let Some((ok, summary)) = result {
                    let (mark, color) = if *ok {
                        ("✓", Color::Green)
                    } else {
                        ("✗", Color::Red)
                    };
                    lines.push(Line::from(Span::styled(
                        format!("  {mark} {summary}"),
                        Style::default().fg(color),
                    )));
                }
            }
            TranscriptEntry::Notice(text) => {
                lines.push(Line::from(Span::styled(
                    format!("• {text}"),
                    Style::default().fg(Color::Magenta).add_modifier(Modifier::ITALIC),
                )));
            }
            TranscriptEntry::Room { from, subject, body } => push_room(&mut lines, from, subject, body),
        }
    }
    lines
}

/// Push a room message: a bold `⇄ from` header (with ` · subject` when one is present) then the body.
fn push_room(lines: &mut Vec<Line<'static>>, from: &str, subject: &str, body: &str) {
    let header = if subject.is_empty() {
        format!("⇄ {from}")
    } else {
        format!("⇄ {from} · {subject}")
    };
    lines.push(Line::from(Span::styled(
        header,
        Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        body.to_string(),
        Style::default().fg(Color::Blue),
    )));
}

/// The transcript paragraph without its border block — shared by the renderer and the scroll-height
/// measurement so `line_count` matches what is drawn.
fn transcript_body(lines: Vec<Line<'static>>) -> Paragraph<'static> {
    Paragraph::new(lines).wrap(Wrap { trim: false })
}

/// Reconcile `app.scroll` / `app.follow` against the real transcript viewport before drawing. While
/// following, pin to the newest content (bottom); otherwise clamp the manual offset and re-engage
/// following once the user has paged back to the bottom. Called from the run loop, which alone knows
/// the terminal size.
pub fn reconcile_scroll(app: &mut App, area: Rect) {
    let transcript = layout(area, !app.roster.is_empty())[0];
    let inner_width = transcript.width.saturating_sub(2);
    let inner_height = transcript.height.saturating_sub(2);
    let total = transcript_body(transcript_lines(app)).line_count(inner_width) as u16;
    let max_scroll = total.saturating_sub(inner_height);
    if app.follow {
        app.scroll = max_scroll;
    } else {
        app.scroll = app.scroll.min(max_scroll);
        if app.scroll >= max_scroll {
            app.follow = true;
        }
    }
}

/// Render the scrollable, wrapped conversation transcript.
fn draw_transcript(frame: &mut Frame, app: &App, area: Rect) {
    let paragraph = transcript_body(transcript_lines(app))
        .block(Block::default().borders(Borders::ALL).title(" transcript "))
        .scroll((app.scroll, 0));
    frame.render_widget(paragraph, area);
}

/// Render the one-line status bar: model, tokens, and in-flight/idle state.
fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let status = &app.status;
    let state = if status.in_flight {
        Span::styled(
            " thinking… ",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )
    } else {
        let reason = status
            .last_reason
            .map(|r| format!(" idle ({r:?}) "))
            .unwrap_or_else(|| " ready ".to_string());
        Span::styled(reason, Style::default().fg(Color::DarkGray))
    };
    let line = Line::from(vec![
        Span::styled(
            format!(" {} ", status.model),
            Style::default().fg(Color::White).bg(Color::Blue),
        ),
        Span::raw(format!(
            "  in {} / out {} tok ",
            status.input_tokens, status.output_tokens
        )),
        state,
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// Render the one-line room bar: the comma-joined display names of the current roster.
fn draw_room_bar(frame: &mut Frame, app: &App, area: Rect) {
    let peers = app
        .roster
        .iter()
        .map(|peer| peer.display.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let line = Line::from(Span::styled(
        format!(" room: {peers} "),
        Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(Paragraph::new(line), area);
}

/// Render the input box, showing the current line with a block cursor.
fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let text = format!("{}\u{2588}", app.input); // trailing full-block as a simple cursor ~keep
    // While a turn is running, Enter is held (the engine does not queue mid-turn) — say so. ~keep
    let title = if app.status.in_flight {
        " message (turn in progress · Esc to cancel · Ctrl-C exit) "
    } else {
        " message (Enter send · Esc quit · Ctrl-C exit) "
    };
    let paragraph = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

/// Render the centered permission-request overlay with key hints.
fn draw_permission_overlay(frame: &mut Frame, prompt: &PermissionPrompt) {
    let area = centered_rect(60, 30, frame.area());
    frame.render_widget(Clear, area);

    let body = vec![
        Line::from(Span::styled(
            format!("{} wants to {}", prompt.tool, prompt.action),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(prompt.target.clone(), Style::default().fg(Color::Yellow))),
        Line::from(""),
        Line::from(Span::styled(
            "[y] allow once   [a] allow for session   [n] deny   [Esc] cancel turn",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let paragraph = Paragraph::new(body)
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" permission required ")
                .border_style(Style::default().fg(Color::Red)),
        );
    frame.render_widget(paragraph, area);
}

/// Push a colored, bold speaker label line, then the markdown-rendered message body.
///
/// The body honors embedded newlines and lightweight markdown via [`render_markdown`], fixing the
/// old single-[`Line`] rendering that mangled multi-line and multi-paragraph replies.
fn push_message(lines: &mut Vec<Line<'static>>, label: &str, color: Color, text: &str) {
    lines.push(Line::from(Span::styled(
        format!("{label}:"),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )));
    lines.extend(render_markdown(text));
}

/// Compute a rectangle `percent_x` × `percent_y` of `area`, centered.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The transcript's max scroll for an 80x24 terminal (transcript inner height is 18 rows).
    fn max_scroll_80x24(app: &App) -> u16 {
        (transcript_body(transcript_lines(app)).line_count(78) as u16).saturating_sub(18)
    }

    fn tall_app() -> App {
        let mut app = App::new("test-model");
        for line in 0..40 {
            app.push_user(format!("entry number {line}"));
        }
        app
    }

    #[test]
    fn following_pins_scroll_to_the_bottom() {
        let mut app = tall_app();
        let expected = max_scroll_80x24(&app);
        reconcile_scroll(&mut app, Rect::new(0, 0, 80, 24));
        assert!(expected > 0, "the fixture must overflow the viewport");
        assert_eq!(app.scroll, expected, "follow mode pins to the newest content");
        assert!(app.follow);
    }

    #[test]
    fn a_manual_offset_below_the_bottom_stays_detached() {
        let mut app = tall_app();
        app.follow = false;
        app.scroll = 3;
        reconcile_scroll(&mut app, Rect::new(0, 0, 80, 24));
        assert_eq!(app.scroll, 3, "an in-range manual offset is left alone");
        assert!(!app.follow, "still detached — the user has not reached the bottom");
    }

    #[test]
    fn scrolling_past_the_bottom_clamps_and_re_engages_follow() {
        let mut app = tall_app();
        app.follow = false;
        app.scroll = u16::MAX;
        reconcile_scroll(&mut app, Rect::new(0, 0, 80, 24));
        assert_eq!(app.scroll, max_scroll_80x24(&app), "offset clamps to the bottom");
        assert!(app.follow, "reaching the bottom re-engages follow");
    }

    #[test]
    fn short_content_never_scrolls() {
        let mut app = App::new("test-model");
        app.push_user("only one short line".into());
        reconcile_scroll(&mut app, Rect::new(0, 0, 80, 24));
        assert_eq!(app.scroll, 0, "content that fits the viewport has no scroll");
    }

    #[test]
    fn a_roster_inserts_a_room_region_and_keeps_the_transcript_at_index_zero() {
        let area = Rect::new(0, 0, 80, 24);
        let without = layout(area, false);
        let with = layout(area, true);
        assert_eq!(without.len(), 3, "no room bar without a roster");
        assert_eq!(with.len(), 4, "a room bar adds one region");
        assert_eq!(with[0].x, without[0].x, "the transcript stays index 0");
        assert!(
            with[0].height < without[0].height,
            "the room bar steals one transcript row"
        );

        let mut app = tall_app();
        app.roster = vec![basemind_agent::RoomPeer {
            id: "a".into(),
            display: "alice".into(),
        }];
        let expected = (transcript_body(transcript_lines(&app)).line_count(78) as u16)
            .saturating_sub(layout(area, true)[0].height.saturating_sub(2));
        reconcile_scroll(&mut app, area);
        assert_eq!(app.scroll, expected, "follow still pins to the bottom with a roster");
        assert!(app.follow);
    }

    #[test]
    fn transcript_lines_render_a_room_message() {
        let mut app = App::new("m");
        app.transcript.push(TranscriptEntry::Room {
            from: "alice".into(),
            subject: String::new(),
            body: "hello team".into(),
        });
        let rendered: String = transcript_lines(&app)
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("alice"), "the sender is rendered");
        assert!(rendered.contains("hello team"), "the body is rendered");
    }
}
