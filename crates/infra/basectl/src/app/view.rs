use crossterm::event::KeyEvent;
use ratatui::{Frame, layout::Rect};

use super::{Action, Resources};
use crate::tui::Keybinding;

/// Trait implemented by each TUI view screen.
pub trait View {
    /// Returns the keybindings available in this view.
    fn keybindings(&self) -> &'static [Keybinding];

    /// Handles a key press event, returning the resulting action.
    fn handle_key(&mut self, key: KeyEvent, resources: &mut Resources) -> Action;

    /// Called each tick to perform periodic updates, returning any resulting action.
    fn tick(&mut self, resources: &mut Resources) -> Action {
        let _ = resources;
        Action::None
    }

    /// Renders this view into the given frame area.
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, resources: &Resources);
}
