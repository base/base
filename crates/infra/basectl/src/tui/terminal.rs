use std::io::{self, Stdout};

use anyhow::Result;
use crossterm::{
    event::DisableMouseCapture,
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::prelude::*;

/// Owns terminal lifecycle and restores terminal state on drop.
#[derive(Debug)]
pub(crate) struct TerminalSession {
    terminal: Option<Terminal<CrosstermBackend<Stdout>>>,
}

impl TerminalSession {
    /// Initializes the terminal session.
    pub(crate) fn new() -> Result<Self> {
        let terminal = setup_terminal()?;
        Ok(Self { terminal: Some(terminal) })
    }

    /// Returns a mutable reference to the inner terminal.
    pub(crate) fn terminal_mut(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        self.terminal.as_mut().expect("terminal session already closed")
    }

    /// Restores terminal state and closes the session.
    pub(crate) fn close(mut self) -> Result<()> {
        if let Some(mut terminal) = self.terminal.take() {
            restore_terminal(&mut terminal)?;
        }
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if let Some(mut terminal) = self.terminal.take() {
            let _ = restore_terminal(&mut terminal);
        }
    }
}

/// Initializes the terminal in raw mode with an alternate screen.
fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, DisableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

/// Restores the terminal to its original state.
fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    Ok(())
}
