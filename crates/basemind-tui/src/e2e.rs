//! End-to-end rendering tests: drive the pure [`App`](crate::app::App) with a scripted sequence of
//! engine events, render it through ratatui's in-memory [`TestBackend`], and assert on the visible
//! frame — the UI half of the scripted-replay smoke, with no terminal and no network.

use basemind_agent::{AgentEvent, StopReason};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::app::App;
use crate::ui;

/// Render `app` into a fixed-size in-memory frame and flatten it to row-joined text for `contains`
/// assertions.
fn render(app: &App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test backend");
    terminal.draw(|frame| ui::draw(frame, app)).expect("draw");
    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..height {
        for x in 0..width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

#[test]
fn a_scripted_turn_renders_markdown_tool_and_status() {
    let mut app = App::new("mock/scripted");
    app.apply(AgentEvent::TurnStarted { turn: 1 });
    app.apply(AgentEvent::TextDelta {
        turn: 1,
        seq: 0,
        text: "# Heading\n\nsome **bold** words".into(),
    });
    app.apply(AgentEvent::ToolStarted {
        turn: 1,
        call_id: "c1".into(),
        name: "code:outline".into(),
        args: serde_json::json!({ "path": "src/lib.rs" }),
    });
    app.apply(AgentEvent::ToolResult {
        call_id: "c1".into(),
        ok: true,
        summary: "12 symbols".into(),
    });
    app.apply(AgentEvent::TurnFinished {
        turn: 1,
        reason: StopReason::Stop,
        steps: 2,
    });

    let screen = render(&app, 80, 24);

    assert!(screen.contains("Heading"), "heading text renders\n{screen}");
    assert!(screen.contains("bold"), "bold text renders\n{screen}");
    assert!(!screen.contains("**bold**"), "bold markers are stripped\n{screen}");
    assert!(!screen.contains("# Heading"), "heading marker is stripped\n{screen}");

    assert!(screen.contains("code:outline"), "tool name renders\n{screen}");
    assert!(screen.contains("12 symbols"), "tool result renders\n{screen}");
    assert!(screen.contains('✓'), "success mark renders\n{screen}");

    assert!(screen.contains("mock/scripted"), "model in status bar\n{screen}");
    assert!(screen.contains("idle (Stop)"), "idle reason in status bar\n{screen}");
}

#[test]
fn a_permission_request_renders_the_overlay() {
    let mut app = App::new("mock/scripted");
    app.apply(AgentEvent::TurnStarted { turn: 1 });
    app.apply(AgentEvent::PermissionRequested {
        turn: 1,
        req_id: 1,
        call_id: "c1".into(),
        tool: "shell:exec".into(),
        action: "exec".into(),
        target: "echo hi".into(),
    });

    let screen = render(&app, 80, 24);
    assert!(
        screen.contains("permission required"),
        "overlay title renders\n{screen}"
    );
    assert!(screen.contains("shell:exec"), "tool name in overlay\n{screen}");
    assert!(screen.contains("echo hi"), "target in overlay\n{screen}");
}
