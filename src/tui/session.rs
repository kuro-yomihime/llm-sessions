use std::io::{self, Write};
use std::time::Duration;

use crossterm::event::{self, Event, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{execute, queue};
use ratatui::backend::CrosstermBackend;
use ratatui::{Terminal, TerminalOptions, Viewport};

use crate::error::{Result, io as io_err};
use crate::tui::app::App;
use crate::tui::keymap::action_for_key;
use crate::tui::ui::{draw, panes};

/// Run the TUI on **stderr**, leaving stdout for the shell-quoted resume
/// command. The user can inspect and paste that command into their shell.
///
/// Returns the resume command the user selected, if any.
pub fn run(app: &mut App) -> Result<Option<Vec<String>>> {
    let mut session = Session::enter()?;
    let outcome = session.event_loop(app);
    // Restore the terminal even when the loop failed; a raw-mode terminal
    // left behind is worse than the original error.
    let restored = session.leave();
    outcome?;
    restored?;
    Ok(app.resume_command().map(<[String]>::to_vec))
}

struct Session {
    terminal: Terminal<CrosstermBackend<io::Stderr>>,
}

impl Session {
    fn enter() -> Result<Self> {
        enable_raw_mode().map_err(io_err("terminal"))?;
        let mut out = io::stderr();
        execute!(out, EnterAlternateScreen).map_err(io_err("terminal"))?;
        let terminal = Terminal::with_options(
            CrosstermBackend::new(io::stderr()),
            TerminalOptions {
                viewport: Viewport::Fullscreen,
            },
        )
        .map_err(io_err("terminal"))?;
        Ok(Self { terminal })
    }

    /// Poll rather than block, so the clock keeps running.
    ///
    /// A blocking `event::read()` froze the view between keystrokes: liveness,
    /// ordering, and a followed transcript all go stale while the user reads.
    /// The tick is short enough to feel live and long enough that an idle TUI
    /// stays off the CPU; [`App::tick`] itself rate-limits the actual rescan.
    fn event_loop(&mut self, app: &mut App) -> Result<()> {
        const TICK: Duration = Duration::from_millis(200);
        loop {
            self.terminal
                .draw(|frame| {
                    let layout = panes(frame.area(), app);
                    app.apply_layout(&layout);
                    draw(frame, app, &layout);
                })
                .map_err(io_err("terminal"))?;
            if app.should_quit() {
                return Ok(());
            }
            if event::poll(TICK).map_err(io_err("terminal"))? {
                match event::read().map_err(io_err("terminal"))? {
                    // Windows reports press and release; acting on both would
                    // double every keystroke.
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if let Some(action) = action_for_key(key, app.prompt()) {
                            app.handle(action);
                        }
                    }
                    // A resize needs no handling: the next frame is drawn
                    // at the new size.
                    _ => {}
                }
            } else {
                app.tick();
            }
        }
    }

    fn leave(&mut self) -> Result<()> {
        let mut out = io::stderr();
        let raw = disable_raw_mode();
        queue!(out, LeaveAlternateScreen).map_err(io_err("terminal"))?;
        out.flush().map_err(io_err("terminal"))?;
        let _ = self.terminal.show_cursor();
        raw.map_err(io_err("terminal"))
    }
}
