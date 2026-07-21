//! The async event loop: pump crossterm input and engine events into the [`App`], and redraw the
//! terminal on a fixed tick so bursts of deltas coalesce into ~30fps frames.
//!
//! This module is the IO shell around the pure core in [`crate::app`]; it is reviewed manually
//! rather than unit-tested. A [`TerminalGuard`] restores the terminal on any exit path, including a
//! panic, so a crash never leaves the user in raw mode / the alternate screen.

use std::io;
use std::time::Duration;

use anyhow::Result;
use basemind_agent::{AgentClient, AgentCommand};
use crossterm::event::{Event, EventStream, KeyEventKind};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use crossterm::{ExecutableCommand, event::DisableMouseCapture};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use tokio::time::interval;

use crate::app::App;
use crate::ui;

/// Redraw cadence — ~30fps. Deltas that arrive between ticks are folded into `App` and rendered
/// together on the next tick.
const FRAME_INTERVAL: Duration = Duration::from_millis(33);

/// RAII guard: leaves raw mode and the alternate screen when dropped, so panics restore the
/// terminal.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        io::stdout().execute(EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = stdout.execute(DisableMouseCapture);
        let _ = stdout.execute(LeaveAlternateScreen);
    }
}

/// Run the UI until the user quits or the engine's event stream ends.
pub async fn run(mut client: impl AgentClient, mut app: App) -> Result<()> {
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;

    let mut input = EventStream::new();
    let mut ticker = interval(FRAME_INTERVAL);
    let mut engine_open = true;

    loop {
        tokio::select! {
            maybe_event = input.next() => match maybe_event {
                Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                    if let Some(command) = app.on_key(key) {
                        client.send_command(command).await?;
                    }
                }
                // A resize does not touch app state but the layout must be recomputed. ~keep
                Some(Ok(Event::Resize(_, _))) => app.dirty = true,
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(error.into()),
                None => break,
            },

            // Engine events: fold into the app. `None` means the engine shut down. ~keep
            event = client.next_event(), if engine_open => match event {
                Some(event) => app.apply(event),
                None => engine_open = false,
            },

            // Coalesced redraw: only when something changed since the last frame, so the ~30fps ~keep
            // ticker does not repaint an idle screen every tick. ~keep
            _ = ticker.tick() => {
                if app.dirty {
                    // Pin/clamp the transcript scroll against the live terminal size before drawing. ~keep
                    let size = terminal.size()?;
                    ui::reconcile_scroll(&mut app, Rect::new(0, 0, size.width, size.height));
                    terminal.draw(|frame| ui::draw(frame, &app))?;
                    app.dirty = false;
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    // Best-effort graceful shutdown; the engine may already be gone. ~keep
    let _ = client.send_command(AgentCommand::Shutdown).await;
    Ok(())
}
