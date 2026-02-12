use crossterm::event::KeyEvent;
use ratatui::{Frame, layout::Rect};

use super::{Action, Resources};
use crate::tui::Keybinding;

pub trait View {
    fn keybindings(&self) -> &'static [Keybinding];

    fn handle_key(&mut self, key: KeyEvent, resources: &mut Resources) -> Action;

    fn tick(&mut self, resources: &mut Resources) -> Action {
        let _ = resources;
        Action::None
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, resources: &Resources);
}
