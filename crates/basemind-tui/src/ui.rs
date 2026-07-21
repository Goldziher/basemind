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

/// Draw the whole UI for the current [`App`] snapshot.
pub fn draw(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),    // transcript
            Constraint::Length(1), // status bar
            Constraint::Length(3), // input box
        ])
        .split(frame.area());

    draw_transcript(frame, app, chunks[0]);
    draw_status(frame, app, chunks[1]);
    draw_input(frame, app, chunks[2]);

    if let Some(prompt) = &app.pending_permission {
        draw_permission_overlay(frame, prompt);
    }
}

/// Render the scrollable, wrapped conversation transcript.
fn draw_transcript(frame: &mut Frame, app: &App, area: Rect) {
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
        }
    }

    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" transcript "))
        .wrap(Wrap { trim: false })
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

/// Render the input box, showing the current line with a block cursor.
fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let text = format!("{}\u{2588}", app.input); // trailing full-block as a simple cursor
    let paragraph = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" message (Enter send · Esc cancel/quit · Ctrl-C exit) "),
        )
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
